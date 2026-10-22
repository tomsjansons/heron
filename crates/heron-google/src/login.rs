//! `heron auth google <id>` and `heron logout google <id>`: the browser
//! sign-in with a loopback redirect, and revoking it again.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, Result};
use console::style;
use heron_core::config::{GoogleAccount, GoogleClient};
use heron_core::secrets;
use indicatif::{ProgressBar, ProgressStyle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::oauth::{self, CALENDAR_SCOPE, MAIL_SCOPE, Pkce, Stored};

/// How long to wait for the user to finish in the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub async fn login(account: &GoogleAccount, client: &GoogleClient) -> Result<()> {
    eprintln!(
        "\n{} {} {}\n",
        style("Signing in").bold().for_stderr(),
        style(&account.id).cyan().bold().for_stderr(),
        style(format!("({})", account.email)).dim().for_stderr(),
    );

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.context("opening a local port for the redirect")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let pkce = Pkce::new();
    let state = oauth::random_token(24);
    let url = oauth::authorize_url(&client.client_id, &redirect_uri, &pkce.challenge, &state, &account.email);

    if open_browser(url.as_str()).await {
        eprintln!("  Opened Google's sign-in page in your browser.");
        eprintln!("  {}", style("If nothing appeared, open this URL:").dim().for_stderr());
    } else {
        eprintln!("  Open this URL in a browser on this machine:");
    }
    eprintln!("\n  {}\n", style(url.as_str()).underlined().for_stderr());
    if std::env::var_os("SSH_CONNECTION").is_some() {
        eprintln!(
            "  {}\n",
            style(format!(
                "Over SSH? Google redirects to 127.0.0.1:{port} on the machine running the browser; \
                 forward it first with `ssh -L {port}:127.0.0.1:{port} <this host>`."
            ))
            .dim()
            .for_stderr()
        );
    }

    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("  {spinner:.cyan} {msg} {elapsed:.dim}")
            .expect("valid template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
    );
    spinner.set_message("Waiting for you to approve access in the browser (Ctrl-C to cancel)");
    spinner.enable_steady_tick(Duration::from_millis(80));

    let callback = tokio::select! {
        result = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_callback(listener, &state)) => result,
        _ = tokio::signal::ctrl_c() => {
            spinner.finish_and_clear();
            anyhow::bail!("sign-in cancelled");
        }
    };
    let Ok(callback) = callback else {
        spinner.finish_and_clear();
        anyhow::bail!(
            "no answer from the browser within {} minutes. If Google showed an error page, check the \
             OAuth client setup (Desktop app client, Gmail and Calendar APIs enabled, your address added \
             as a test user while the app is in \"Testing\")",
            LOGIN_TIMEOUT.as_secs() / 60
        );
    };
    let (mut browser, code) = callback?;

    spinner.set_message("Finishing sign-in with Google");
    let outcome = tokio::select! {
        outcome = finish(account, client, code, &pkce, &redirect_uri) => outcome,
        _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("sign-in cancelled")),
    };
    let page = match &outcome {
        Ok(stored) => html_page(true, "Signed in", &format!("heron can now sync {} for {}.", granted_kinds(stored), stored.email)),
        Err(err) => html_page(false, "Sign-in failed", &format!("{err:#}")),
    };
    respond(&mut browser, "200 OK", &page).await;
    spinner.finish_and_clear();

    let stored = outcome?;
    print_summary(account, &stored);
    Ok(())
}

/// Exchanges the code, checks who signed in and stores the refresh token.
async fn finish(
    account: &GoogleAccount,
    client: &GoogleClient,
    code: Result<String>,
    pkce: &Pkce,
    redirect_uri: &str,
) -> Result<Stored> {
    let code = code?;
    let http = oauth::http_client()?;
    let token = oauth::exchange_code(&http, client, &code, &pkce.verifier, redirect_uri).await?;
    let email = oauth::signed_in_email(&http, &token).await.context("finding out which account signed in")?;

    if !email.eq_ignore_ascii_case(&account.email) {
        // Do not leave a grant behind for an account heron will not use.
        let _ = oauth::revoke(&http, token.refresh_token.as_deref().unwrap_or(&token.access_token)).await;
        anyhow::bail!(
            "signed in as {email}, but account {:?} is configured for {}. Run the command again and pick \
             {} in Google's account chooser, or change `email` for this account in config.toml",
            account.id,
            account.email,
            account.email
        );
    }
    let refresh_token = token.refresh_token.clone().context(
        "Google did not return a refresh token; remove heron's access at \
         https://myaccount.google.com/permissions and try again",
    )?;
    let stored = Stored { refresh_token, scopes: token.scopes(), email };
    secrets::store(&oauth::secret_name(&account.id), &stored).context("saving the credentials")?;
    Ok(stored)
}

fn granted_kinds(stored: &Stored) -> &'static str {
    match (stored.has(MAIL_SCOPE), stored.has(CALENDAR_SCOPE)) {
        (true, true) => "Gmail and Google Calendar",
        (true, false) => "Gmail",
        (false, true) => "Google Calendar",
        (false, false) => "nothing",
    }
}

fn print_summary(account: &GoogleAccount, stored: &Stored) {
    let ok = style("✓").green().bold().for_stderr();
    eprintln!("  {ok} Signed in as {}\n", style(&stored.email).bold().for_stderr());

    let mut missing = false;
    let rows = [("Gmail (IMAP)", MAIL_SCOPE, account.mail), ("Google Calendar", CALENDAR_SCOPE, account.calendar)];
    for (name, scope, wanted) in rows {
        let line = match (stored.has(scope), wanted) {
            (true, _) => format!("{ok} granted"),
            (false, true) => {
                missing = true;
                format!("{} not granted", style("✗").red().bold().for_stderr())
            }
            (false, false) => format!("{}", style("– not granted (disabled in config)").dim().for_stderr()),
        };
        eprintln!("    {name:<18}{line}");
    }
    if missing {
        eprintln!(
            "\n  {} Some access was left unticked on Google's consent screen, so that part will show\n    \
             \"login required\". Run `heron auth google {}` again and tick every box.",
            style("!").yellow().bold().for_stderr(),
            account.id
        );
    }
    eprintln!(
        "\n  {}\n  {}\n",
        style(format!("Saved to {}", secrets::path(&oauth::secret_name(&account.id)).display())).dim().for_stderr(),
        style("Tip: while the OAuth consent screen is in \"Testing\", Google expires this login after 7 days; \
               set it to \"In production\" to keep it.")
            .dim()
            .for_stderr(),
    );
}

pub async fn logout(account: &GoogleAccount) -> Result<()> {
    let name = oauth::secret_name(&account.id);
    let Some(stored) = secrets::load::<Stored>(&name)? else {
        eprintln!("{} is not signed in.", account.id);
        return Ok(());
    };
    let http = oauth::http_client()?;
    match oauth::revoke(&http, &stored.refresh_token).await {
        Ok(()) => eprintln!("Revoked heron's access to {}.", stored.email),
        Err(err) => eprintln!(
            "{} could not revoke the token ({err:#}).\n  Remove heron at https://myaccount.google.com/permissions if needed.",
            style("warning:").yellow().bold().for_stderr()
        ),
    }
    secrets::remove(&name)?;
    eprintln!("Signed out {}.", account.id);
    Ok(())
}

/// Tries `xdg-open`. False when there is no graphical session or it failed,
/// so the caller tells the user to open the URL themselves.
async fn open_browser(url: &str) -> bool {
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    let child = tokio::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    // Some openers return at once, others only when the browser exits.
    match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(Ok(status)) => status.success(),
        Ok(Err(_)) => false,
        Err(_) => true,
    }
}

/// What a request to the loopback server means.
#[derive(Debug, PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// Google redirected with an error, such as the user pressing Cancel.
    Denied(String),
    /// A redirect from an older sign-in attempt.
    StateMismatch,
    /// Favicon and other stray requests.
    Ignore,
}

/// Interprets the head of an HTTP request sent to the redirect address.
pub fn parse_callback(head: &str, state: &str) -> Callback {
    let Some(target) = head.lines().next().and_then(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some("GET")).then(|| parts.next()).flatten()
    }) else {
        return Callback::Ignore;
    };
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignore;
    };
    if url.path() != "/" {
        return Callback::Ignore;
    }
    let (mut code, mut got_state, mut error, mut description) = (None, None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => got_state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => description = Some(value.into_owned()),
            _ => {}
        }
    }
    if got_state.is_some() && got_state.as_deref() != Some(state) {
        return Callback::StateMismatch;
    }
    if let Some(error) = error {
        return Callback::Denied(match (error.as_str(), description) {
            ("access_denied", _) => "access was denied on Google's consent screen".to_owned(),
            (_, Some(description)) => format!("{error}: {description}"),
            (_, None) => error,
        });
    }
    match (code, got_state) {
        (Some(code), Some(_)) => Callback::Code(code),
        _ => Callback::Ignore,
    }
}

/// Serves the redirect address until Google sends the user back. Returns the
/// browser's connection, still open, so the final page can say how it went.
async fn wait_for_callback(listener: TcpListener, state: &str) -> Result<(TcpStream, Result<String>)> {
    // Browsers open speculative connections that never send a request, so
    // each connection is read on its own task.
    let (tx, mut rx) = mpsc::channel::<(TcpStream, String)>(8);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting the browser's connection")?;
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Ok(Some((stream, head))) = tokio::time::timeout(Duration::from_secs(60), read_head(stream)).await {
                        let _ = tx.send((stream, head)).await;
                    }
                });
            }
            Some((mut stream, head)) = rx.recv() => match parse_callback(&head, state) {
                Callback::Code(code) => return Ok((stream, Ok(code))),
                Callback::Denied(reason) => return Ok((stream, Err(anyhow::anyhow!("Google sign-in failed: {reason}")))),
                Callback::StateMismatch => {
                    let page = html_page(false, "Outdated sign-in link", "This tab belongs to an earlier sign-in attempt. Use the link printed most recently in the terminal.");
                    respond(&mut stream, "400 Bad Request", &page).await;
                }
                Callback::Ignore => respond(&mut stream, "404 Not Found", "").await,
            },
        }
    }
}

async fn read_head(mut stream: TcpStream) -> Option<(TcpStream, String)> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some((stream, String::from_utf8_lossy(&buf).into_owned()))
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The small page shown in the browser tab after the redirect.
pub fn html_page(ok: bool, title: &str, message: &str) -> String {
    let (icon, accent) = if ok { ("✓", "#1e8e3e") } else { ("!", "#d93025") };
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>heron · {title}</title>
<style>
:root{{color-scheme:light dark;--bg:#f3f4f6;--card:#fff;--fg:#1f2328;--muted:#5f6670;--accent:{accent}}}
@media (prefers-color-scheme:dark){{:root{{--bg:#15171a;--card:#1f2227;--fg:#e8eaed;--muted:#9aa0a6}}}}
body{{margin:0;min-height:100vh;display:grid;place-items:center;background:var(--bg);color:var(--fg);font:16px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}}
main{{background:var(--card);margin:16px;padding:40px 44px;border-radius:18px;max-width:30rem;text-align:center;box-shadow:0 6px 30px rgba(0,0,0,.08)}}
.icon{{width:56px;height:56px;margin:0 auto 18px;border-radius:50%;display:grid;place-items:center;background:var(--accent);color:#fff;font-size:30px;font-weight:700}}
h1{{margin:0 0 8px;font-size:1.35rem}}
p{{margin:6px 0;color:var(--muted);overflow-wrap:anywhere}}
.brand{{margin-top:22px;font-size:.85rem;letter-spacing:.04em;opacity:.7}}
</style></head>
<body><main><div class="icon">{icon}</div><h1>{title}</h1><p>{message}</p>
<p>You can close this tab and return to the terminal.</p><p class="brand">heron</p></main></body></html>
"#,
        title = escape(title),
        message = escape(message),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_with_code() {
        let head = "GET /?state=abc&code=4%2F0Adeu&scope=email%20openid HTTP/1.1\r\nHost: 127.0.0.1:5000\r\n\r\n";
        assert_eq!(parse_callback(head, "abc"), Callback::Code("4/0Adeu".into()));
    }

    #[test]
    fn callback_errors_and_strays() {
        let denied = "GET /?error=access_denied&state=abc HTTP/1.1\r\n\r\n";
        assert!(matches!(parse_callback(denied, "abc"), Callback::Denied(r) if r.contains("denied")));
        let stale = "GET /?state=old&code=x HTTP/1.1\r\n\r\n";
        assert_eq!(parse_callback(stale, "abc"), Callback::StateMismatch);
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1\r\n\r\n", "abc"), Callback::Ignore);
        assert_eq!(parse_callback("GET / HTTP/1.1\r\n\r\n", "abc"), Callback::Ignore);
        assert_eq!(parse_callback("POST /?code=x&state=abc HTTP/1.1\r\n\r\n", "abc"), Callback::Ignore);
        assert_eq!(parse_callback("", "abc"), Callback::Ignore);
    }

    #[tokio::test]
    async fn loopback_server_survives_idle_and_stray_connections() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { wait_for_callback(listener, "st").await });

        // A speculative connection that never sends anything.
        let _idle = TcpStream::connect(addr).await.unwrap();
        let mut favicon = TcpStream::connect(addr).await.unwrap();
        favicon.write_all(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await.unwrap();
        let mut answer = String::new();
        favicon.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 404"));

        let mut browser = TcpStream::connect(addr).await.unwrap();
        browser.write_all(b"GET /?code=abc&state=st HTTP/1.1\r\nHost: x\r\n\r\n").await.unwrap();
        let (mut stream, code) = server.await.unwrap().unwrap();
        assert_eq!(code.unwrap(), "abc");
        respond(&mut stream, "200 OK", &html_page(true, "Signed in", "ok")).await;
        let mut page = String::new();
        browser.read_to_string(&mut page).await.unwrap();
        assert!(page.starts_with("HTTP/1.1 200 OK") && page.contains("Signed in"));
    }

    #[test]
    fn page_escapes_message() {
        let page = html_page(false, "Sign-in failed", "<script>x</script>");
        assert!(page.contains("&lt;script&gt;"));
        assert!(!page.contains("<script>"));
    }
}
