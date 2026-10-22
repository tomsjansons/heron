//! heron: CLI for herond. Talks to the daemon over its socket, and runs the
//! interactive logins that store credentials for it.

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use heron_core::{Config, Request, Response, Snapshot, SourceState, paths, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(version, about = "Mail, calendar and Discord status from herond")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show counts, upcoming events and source health.
    Status {
        /// Print the raw snapshot as one JSON line.
        #[arg(long)]
        json: bool,
    },
    /// Print the snapshot as JSON, then a new line on every change.
    Watch,
    /// Clear "new" for one account, or every account.
    Ack { account: Option<String> },
    /// Mark a message read on the server.
    MarkRead { account: String, id: String },
    /// Mark a message unread on the server.
    MarkUnread { account: String, id: String },
    /// Re-read the config and restart every source.
    Reload,
    /// Log in to a source and store its credentials.
    Auth {
        provider: AuthProvider,
        /// Account id from the config. Not used for discord.
        account: Option<String>,
    },
    /// Revoke and delete a source's stored credentials.
    Logout {
        provider: AuthProvider,
        account: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthProvider {
    Google,
    Proton,
    Discord,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Status { json } => {
            let line = request_one(&Request::Status).await?;
            if json {
                println!("{line}");
            } else if let Response::Snapshot(snapshot) = serde_json::from_str(&line)? {
                print_status(&snapshot);
            }
        }
        Command::Watch => watch().await?,
        Command::Ack { account } => ok(Request::Ack { account }).await?,
        Command::MarkRead { account, id } => ok(Request::MarkRead { account, id }).await?,
        Command::MarkUnread { account, id } => ok(Request::MarkUnread { account, id }).await?,
        Command::Reload => ok(Request::Reload).await?,
        Command::Auth { provider, account } => {
            auth(provider, account.as_deref(), false).await?;
            reload_if_running().await;
        }
        Command::Logout { provider, account } => {
            auth(provider, account.as_deref(), true).await?;
            reload_if_running().await;
        }
    }
    Ok(())
}

async fn auth(provider: AuthProvider, account: Option<&str>, logout: bool) -> Result<()> {
    let config = Config::load()?;
    let account_id = || {
        account.context("which account? pass the id from the config, e.g. `heron auth google personal`")
    };
    match provider {
        AuthProvider::Google => {
            let account = config.google_account(account_id()?)?;
            if logout {
                return heron_google::logout(account).await;
            }
            let client = config.google_client.as_ref().with_context(|| {
                format!(
                    "add a [google_client] section with client_id and client_secret to {}\n\
                     (Google Cloud console → APIs & Services → Credentials → OAuth client ID → Desktop app)",
                    paths::config_file().display()
                )
            })?;
            heron_google::login(account, client).await
        }
        AuthProvider::Proton => {
            let account = config.proton_account(account_id()?)?;
            if logout { heron_proton::logout(account).await } else { heron_proton::login(account).await }
        }
        AuthProvider::Discord => {
            if logout { heron_discord::logout().await } else { heron_discord::login().await }
        }
    }
}

/// After a login, a running daemon picks up the new credentials.
async fn reload_if_running() {
    if UnixStream::connect(socket_path()).await.is_ok() {
        match ok(Request::Reload).await {
            Ok(()) => eprintln!("herond reloaded"),
            Err(err) => eprintln!("herond reload failed: {err:#}"),
        }
    }
}

type Lines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;

/// Returns the write half too: dropping it would end a watch.
async fn connect(request: &Request) -> Result<(Lines, tokio::net::unix::OwnedWriteHalf)> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("herond is not running (no socket at {})", path.display()))?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok((BufReader::new(read).lines(), write))
}

async fn request_one(request: &Request) -> Result<String> {
    let (mut lines, _write) = connect(request).await?;
    lines.next_line().await?.context("herond closed the connection")
}

/// Sends a request that answers `ok` or an error.
async fn ok(request: Request) -> Result<()> {
    match serde_json::from_str(&request_one(&request).await?)? {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!(message),
        Response::Snapshot(_) => anyhow::bail!("unexpected snapshot"),
    }
}

async fn watch() -> Result<()> {
    let (mut lines, _write) = connect(&Request::Watch).await?;
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
    }
    Ok(())
}

fn print_status(snapshot: &Snapshot) {
    let style = Style::new();

    println!("{}", style.bold(&format!("Mail  {} new · {} unread", snapshot.new, snapshot.unread)));
    for account in &snapshot.mail {
        println!("  {:<12} {:>3} new {:>4} unread", account.id, account.new, account.unread);
        for message in account.messages.iter().filter(|m| m.new).take(5) {
            println!("    {} {}  {}", style.accent("●"), truncate(&message.from, 24), truncate(&message.subject, 60));
        }
    }

    println!("\n{}", style.bold("Agenda"));
    if snapshot.calendar.is_empty() {
        println!("  {}", style.dim("nothing scheduled"));
    }
    for event in snapshot.calendar.iter().take(10) {
        println!("  {}  {}  {}", when(event.start, event.all_day), truncate(&event.title, 50), style.dim(&event.calendar));
    }

    if let Some(guild) = &snapshot.discord {
        println!(
            "\n{}",
            style.bold(&format!("Discord  {}: {} unread channels · {} mentions", guild.name, guild.unread_channels, guild.mentions))
        );
        for channel in guild.channels.iter().take(8) {
            let mentions = if channel.mentions > 0 { format!(" @{}", channel.mentions) } else { String::new() };
            println!("  #{}{}", channel.name, style.accent(&mentions));
        }
    }

    let problems: Vec<_> = snapshot.sources.iter().filter(|s| s.state != SourceState::Ok).collect();
    if !problems.is_empty() {
        println!("\n{}", style.bold("Sources"));
        for source in problems {
            let hint = match source.state {
                SourceState::NeedsAuth => "login required, run `heron auth`".to_owned(),
                _ => source.message.clone().unwrap_or_default(),
            };
            println!("  {:<12} {:?} {:?}  {}", source.id, source.kind, source.state, style.dim(&hint));
        }
    }
}

fn when(unix: i64, all_day: bool) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(unix) else { return unix.to_string() };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    let today = jiff::Zoned::now().date();
    let day = match (zoned.date() - today).get_days() {
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        _ => zoned.strftime("%a %d %b").to_string(),
    };
    if all_day { format!("{day:<10} {:>5}", "") } else { format!("{day:<10} {}", zoned.strftime("%H:%M")) }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// ANSI styling only when stdout is a terminal and NO_COLOR is unset.
struct Style(bool);

impl Style {
    fn new() -> Self {
        Self(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }
    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }
    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
    fn accent(&self, text: &str) -> String {
        self.wrap("36", text)
    }
}
