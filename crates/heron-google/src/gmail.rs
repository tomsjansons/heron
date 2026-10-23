//! Gmail INBOX over IMAP with SASL XOAUTH2.
//!
//! Two connections: one sits in IDLE and only says "something changed", the
//! other runs searches, fetches and flag changes, so commands never have to
//! wait for IDLE to be interrupted.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use async_imap::extensions::idle::IdleResponse;
use async_imap::imap_proto::Address;
use async_imap::types::{Fetch, NameAttribute};
use futures_util::TryStreamExt as _;
use heron_core::config::GoogleAccount;
use heron_core::{Hub, MailCommand, MailUpdate, Message, MessageBody, Provider, SourceKind, SourceState, mailbody};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;
use tracing::{debug, info, warn};

use crate::oauth::Tokens;
use crate::{Backoff, short};

const HOST: &str = "imap.gmail.com";
const PORT: u16 = 993;
/// Gmail drops IDLE after about 29 minutes; start a fresh one well before.
const IDLE_REISSUE: Duration = Duration::from_secs(25 * 60);
/// Changes arrive in bursts (EXISTS, FETCH, EXPUNGE); wait for quiet.
const DEBOUNCE: Duration = Duration::from_millis(400);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds every command on the worker connection.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

type Session = async_imap::Session<TlsStream<TcpStream>>;

/// Keeps the INBOX in the hub until the login stops working. Only returns an
/// error, which is [`crate::oauth::NeedsAuth`] unless something is badly wrong.
pub async fn run(account: GoogleAccount, max_messages: usize, tokens: Arc<Tokens>, hub: Hub) -> Result<()> {
    let mut commands = Commands { rx: hub.register_mail(&account.id), open: true };
    let mut cache = Cache::default();
    let mut backoff = Backoff::default();
    let ctx = Ctx { account: &account, max_messages, tokens: &tokens, hub: &hub };
    loop {
        let err = ctx.connected(&mut commands, &mut cache, &mut backoff).await;
        if crate::oauth::needs_auth(&err).is_some() {
            return Err(err);
        }
        let delay = backoff.next();
        warn!("google/{} gmail: {err:#}; retrying in {}s", account.id, delay.as_secs());
        hub.set_status(&account.id, SourceKind::Gmail, SourceState::Error, Some(short(&err)));
        commands.reject_for(delay).await;
    }
}

struct Ctx<'a> {
    account: &'a GoogleAccount,
    max_messages: usize,
    tokens: &'a Tokens,
    hub: &'a Hub,
}

impl Ctx<'_> {
    /// One connected stretch; returns why it ended.
    async fn connected(&self, commands: &mut Commands, cache: &mut Cache, backoff: &mut Backoff) -> anyhow::Error {
        match self.connected_inner(commands, cache, backoff).await {
            Ok(never) => match never {},
            Err(err) => err,
        }
    }

    async fn connected_inner(
        &self,
        commands: &mut Commands,
        cache: &mut Cache,
        backoff: &mut Backoff,
    ) -> Result<std::convert::Infallible> {
        let mut worker = connect(self.account, self.tokens).await?;
        let mailbox = within(worker.select("INBOX")).await.context("selecting INBOX")?;
        cache.validate(mailbox.uid_validity);
        let mut watcher = connect(self.account, self.tokens).await?;
        within(watcher.select("INBOX")).await.context("selecting INBOX")?;

        self.publish(&mut worker, cache).await?;
        info!("google/{} gmail: connected", self.account.id);
        backoff.reset();

        let mut idle = watcher.idle();
        within(idle.init()).await.context("starting IDLE")?;
        let mut reissue_at = Instant::now() + IDLE_REISSUE;

        loop {
            enum Wake {
                Changed,
                Reissue,
                Command(MailCommand),
            }
            let wake = {
                let (wait, _stop) = idle.wait_with_timeout(IDLE_REISSUE + Duration::from_secs(120));
                tokio::select! {
                    result = wait => match result.context("waiting in IDLE")? {
                        IdleResponse::NewData(_) => Wake::Changed,
                        IdleResponse::Timeout => Wake::Reissue,
                        IdleResponse::ManualInterrupt => anyhow::bail!("IMAP connection closed"),
                    },
                    _ = sleep_until(reissue_at) => Wake::Reissue,
                    Some(command) = commands.recv() => Wake::Command(command),
                }
            };
            match wake {
                Wake::Changed => {
                    // Let the burst finish, then look once.
                    loop {
                        let (wait, _stop) = idle.wait_with_timeout(DEBOUNCE);
                        match wait.await.context("waiting in IDLE")? {
                            IdleResponse::NewData(_) => continue,
                            IdleResponse::Timeout => break,
                            IdleResponse::ManualInterrupt => anyhow::bail!("IMAP connection closed"),
                        }
                    }
                    debug!("google/{} gmail: mailbox changed", self.account.id);
                }
                Wake::Reissue => {
                    let session = within(idle.done()).await.context("ending IDLE")?;
                    idle = session.idle();
                    within(idle.init()).await.context("restarting IDLE")?;
                    reissue_at = Instant::now() + IDLE_REISSUE;
                }
                Wake::Command(command) => {
                    let (id, seen, reply) = match command {
                        MailCommand::MarkRead { id, reply } => (id, true, reply),
                        MailCommand::MarkUnread { id, reply } => (id, false, reply),
                        MailCommand::Trash { id, reply } => {
                            let _ = reply.send(trash(&mut worker, &id).await);
                            self.publish(&mut worker, cache).await?;
                            continue;
                        }
                        MailCommand::Fetch { id, reply } => {
                            // Read state is untouched, so no refresh; a broken
                            // connection shows up in IDLE or the next command.
                            let _ = reply.send(fetch_body(&mut worker, &id).await);
                            continue;
                        }
                    };
                    // A refused change leaves the connection usable; a broken
                    // connection shows up in the refresh below.
                    let _ = reply.send(set_seen(&mut worker, &id, seen).await);
                }
            }
            self.publish(&mut worker, cache).await?;
        }
    }

    async fn publish(&self, worker: &mut Session, cache: &mut Cache) -> Result<()> {
        let update = refresh(worker, cache, self.max_messages, &self.account.email).await?;
        self.hub.set_mail(&self.account.id, Provider::Gmail, update);
        self.hub.set_status(&self.account.id, SourceKind::Gmail, SourceState::Ok, None);
        Ok(())
    }
}

/// The hub's command channel; replies "not connected" while reconnecting.
struct Commands {
    rx: mpsc::Receiver<MailCommand>,
    open: bool,
}

impl Commands {
    /// Next command, or pending forever once the hub dropped the channel.
    async fn recv(&mut self) -> Option<MailCommand> {
        if self.open {
            let command = self.rx.recv().await;
            self.open = command.is_some();
            if command.is_some() {
                return command;
            }
        }
        std::future::pending().await
    }

    async fn reject_for(&mut self, delay: Duration) {
        let deadline = Instant::now() + delay;
        loop {
            tokio::select! {
                _ = sleep_until(deadline) => return,
                Some(command) = self.recv() => {
                    command.fail(anyhow::anyhow!("Gmail is reconnecting; try again shortly"));
                }
            }
        }
    }
}

/// Envelopes already fetched, valid for one UIDVALIDITY.
#[derive(Default)]
struct Cache {
    uid_validity: Option<u32>,
    messages: HashMap<u32, Message>,
}

impl Cache {
    /// UIDs from another UIDVALIDITY name different messages; forget them.
    fn validate(&mut self, uid_validity: Option<u32>) {
        if self.uid_validity != uid_validity {
            if self.uid_validity.is_some() {
                info!("INBOX UIDVALIDITY changed; refetching");
            }
            self.messages.clear();
            self.uid_validity = uid_validity;
        }
    }
}

async fn within<T, E>(future: impl Future<Output = Result<T, E>>) -> Result<T>
where
    E: std::error::Error + Send + Sync + 'static,
{
    Ok(timeout(COMMAND_TIMEOUT, future).await.context("IMAP server stopped answering")??)
}

fn tls_connector() -> Result<TlsConnector> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    if let Some(config) = CONFIG.get() {
        return Ok(TlsConnector::from(config.clone()));
    }
    use rustls_platform_verifier::BuilderVerifierExt as _;
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_platform_verifier()?
        .with_no_client_auth();
    Ok(TlsConnector::from(CONFIG.get_or_init(|| Arc::new(config)).clone()))
}

/// Opens an authenticated IMAP session, fetching a fresh access token if needed.
/// A healthy XOAUTH2 login answers in about two seconds, but async-imap's
/// AUTHENTICATE now and then stalls waiting for a response that a fresh
/// connection gets at once. So a stalled login is abandoned quickly and retried.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
const LOGIN_ATTEMPTS: u32 = 3;

async fn connect(account: &GoogleAccount, tokens: &Tokens) -> Result<Session> {
    for attempt in 1..LOGIN_ATTEMPTS {
        match connect_once(account, tokens).await {
            Err(err) if err.is::<LoginStalled>() => {
                debug!("google/{} gmail: IMAP login stalled (attempt {attempt}); reconnecting", account.id);
            }
            result => return result,
        }
    }
    connect_once(account, tokens).await
}

#[derive(Debug)]
struct LoginStalled;

impl std::fmt::Display for LoginStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IMAP login timed out")
    }
}

impl std::error::Error for LoginStalled {}

async fn connect_once(account: &GoogleAccount, tokens: &Tokens) -> Result<Session> {
    let token = tokens.access_token().await?;
    let tcp = timeout(CONNECT_TIMEOUT, TcpStream::connect((HOST, PORT))).await
        .context("timed out connecting to imap.gmail.com")?
        .context("connecting to imap.gmail.com")?;
    let name = rustls::pki_types::ServerName::try_from(HOST)?;
    let tls = timeout(CONNECT_TIMEOUT, tls_connector()?.connect(name, tcp)).await
        .context("timed out in the TLS handshake")?
        .context("TLS handshake with imap.gmail.com")?;

    let mut client = async_imap::Client::new(tls);
    within(async { client.read_response().await.map_err(async_imap::error::Error::from) }).await
        .context("reading the IMAP greeting")?
        .context("imap.gmail.com closed the connection")?;

    let auth = XOAuth2 { user: account.email.clone(), token, sent: false };
    match timeout(LOGIN_TIMEOUT, client.authenticate("XOAUTH2", auth)).await {
        Ok(Ok(session)) => Ok(session),
        Ok(Err((err, _))) => {
            // Most likely an expired or revoked access token; fetch a new one next time.
            tokens.invalidate().await;
            Err(anyhow::anyhow!("Gmail rejected the IMAP login: {err}"))
        }
        Err(_) => Err(LoginStalled.into()),
    }
}

/// SASL XOAUTH2. After a failure Gmail sends a JSON challenge that must be
/// answered with an empty response before the tagged NO arrives.
struct XOAuth2 {
    user: String,
    token: String,
    sent: bool,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, _challenge: &[u8]) -> String {
        if std::mem::replace(&mut self.sent, true) {
            return String::new();
        }
        format!("user={}\x01auth=Bearer {}\x01\x01", self.user, self.token)
    }
}

/// The unread count and the newest unread envelopes, fetching only new ones.
async fn refresh(worker: &mut Session, cache: &mut Cache, max_messages: usize, email: &str) -> Result<MailUpdate> {
    let unseen = within(worker.uid_search("UNSEEN")).await.context("searching unread mail")?;
    let mut uids: Vec<u32> = unseen.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));
    let unread = uids.len() as u32;
    uids.truncate(max_messages);

    cache.messages.retain(|uid, _| uids.contains(uid));
    let missing: Vec<String> = uids.iter().filter(|uid| !cache.messages.contains_key(uid)).map(u32::to_string).collect();
    if !missing.is_empty() {
        let fetched: Vec<Fetch> = within(async {
            worker.uid_fetch(missing.join(","), "(UID FLAGS INTERNALDATE ENVELOPE X-GM-MSGID)").await?
                .try_collect::<Vec<_>>().await
        })
        .await
        .context("fetching envelopes")?;
        for fetch in &fetched {
            if let Some(uid) = fetch.uid {
                cache.messages.insert(uid, message(fetch, uid, email));
            }
        }
    }

    let mut messages: Vec<Message> = uids.iter().filter_map(|uid| cache.messages.get(uid).cloned()).collect();
    messages.sort_by(|a, b| b.received.cmp(&a.received));
    Ok(MailUpdate { unread, messages })
}

fn message(fetch: &Fetch, uid: u32, email: &str) -> Message {
    let envelope = fetch.envelope();
    let subject = envelope.and_then(|e| e.subject.as_deref()).map(decode_header).unwrap_or_default();
    let from = envelope.and_then(|e| e.from.as_deref()).map(format_from).unwrap_or_default();
    Message {
        id: uid.to_string(),
        from,
        subject: if subject.is_empty() { "(no subject)".into() } else { subject },
        received: fetch.internal_date().map(|d| d.timestamp()).unwrap_or_else(heron_core::now),
        unread: true,
        new: false,
        url: fetch.gmail_msg_id().map(|id| message_url(email, *id)),
    }
}

pub fn message_url(email: &str, gm_msgid: u64) -> String {
    format!("https://mail.google.com/mail/u/{email}/#all/{gm_msgid:x}")
}

/// Decodes RFC 2047 encoded words and unfolds the header onto one line.
pub fn decode_header(raw: &[u8]) -> String {
    let text = rfc2047_decoder::decode(raw).unwrap_or_else(|_| String::from_utf8_lossy(raw).into_owned());
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The sender's display name, or the bare address when there is none.
fn format_from(addresses: &[Address<'_>]) -> String {
    let Some(address) = addresses.first() else { return String::new() };
    let name = address.name.as_deref().map(decode_header).unwrap_or_default();
    let name = name.trim_matches('"').trim();
    if !name.is_empty() {
        return name.to_owned();
    }
    let part = |p: &Option<std::borrow::Cow<'_, [u8]>>| p.as_deref().map(String::from_utf8_lossy).unwrap_or_default().into_owned();
    match (part(&address.mailbox), part(&address.host)) {
        (mailbox, host) if host.is_empty() => mailbox,
        (mailbox, host) => format!("{mailbox}@{host}"),
    }
}

/// The whole message with `BODY.PEEK[]`, which leaves the `\Seen` flag alone.
async fn fetch_body(worker: &mut Session, id: &str) -> Result<MessageBody> {
    let uid: u32 = id.parse().with_context(|| format!("not a Gmail message id: {id:?}"))?;
    let fetched: Vec<Fetch> = within(async {
        worker.uid_fetch(uid.to_string(), "(UID BODY.PEEK[])").await?.try_collect::<Vec<_>>().await
    })
    .await
    .with_context(|| format!("fetching message {uid}"))?;
    let raw = fetched.iter()
        .find(|f| f.uid == Some(uid))
        .and_then(Fetch::body)
        .with_context(|| format!("message {uid} is no longer in the INBOX"))?;
    debug!("gmail: fetched message {uid} ({} bytes)", raw.len());
    let mut body = mailbody::from_mime(raw).with_context(|| format!("parsing message {uid}"))?;
    body.id = id.to_owned();
    Ok(body)
}

/// Moves a message from the INBOX to the mailbox flagged `\Trash`, whose name
/// depends on the account's language ("[Gmail]/Trash", "[Gmail]/Bin", ...).
async fn trash(worker: &mut Session, id: &str) -> Result<()> {
    let uid: u32 = id.parse().with_context(|| format!("not a Gmail message id: {id:?}"))?;
    let names: Vec<_> = within(async { worker.list(Some(""), Some("*")).await?.try_collect::<Vec<_>>().await }).await
        .context("listing mailboxes")?;
    let trash = names.iter()
        .find(|name| name.attributes().iter().any(|a| matches!(a, NameAttribute::Trash)))
        .map(|name| name.name().to_owned())
        .context("this account has no mailbox flagged as Trash")?;
    within(worker.uid_mv(uid.to_string(), &trash)).await
        .with_context(|| format!("moving message {uid} to {trash}"))?;
    Ok(())
}

async fn set_seen(worker: &mut Session, id: &str, seen: bool) -> Result<()> {
    let uid: u32 = id.parse().with_context(|| format!("not a Gmail message id: {id:?}"))?;
    let change = if seen { "+FLAGS.SILENT (\\Seen)" } else { "-FLAGS.SILENT (\\Seen)" };
    within(async { worker.uid_store(uid.to_string(), change).await?.try_collect::<Vec<_>>().await }).await
        .with_context(|| format!("marking message {uid} {}", if seen { "read" } else { "unread" }))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_encoded_words() {
        assert_eq!(decode_header(b"=?UTF-8?B?w4lsw6h2ZSBwcsOpc2VudA==?="), "Élève présent");
        assert_eq!(decode_header(b"=?ISO-8859-1?Q?Caf=E9_ouvert?="), "Café ouvert");
        assert_eq!(decode_header(b"Re: =?utf-8?q?R=C4=ABga?= meetup"), "Re: Rīga meetup");
        assert_eq!(decode_header(b"plain\r\n  folded subject"), "plain folded subject");
        assert_eq!(decode_header("raw ütf8".as_bytes()), "raw ütf8");
    }

    #[test]
    fn from_prefers_name() {
        use std::borrow::Cow;
        let address = |name: Option<&'static str>| Address {
            name: name.map(|n| Cow::Borrowed(n.as_bytes())),
            adl: None,
            mailbox: Some(Cow::Borrowed(b"jane")),
            host: Some(Cow::Borrowed(b"example.com")),
        };
        assert_eq!(format_from(&[address(Some("=?UTF-8?Q?J=C4=81nis?="))]), "Jānis");
        assert_eq!(format_from(&[address(None)]), "jane@example.com");
        assert_eq!(format_from(&[address(Some(""))]), "jane@example.com");
        assert_eq!(format_from(&[]), "");
    }

    #[test]
    fn url_uses_hex_message_id() {
        assert_eq!(
            message_url("me@gmail.com", 1_278_455_344_230_334_865),
            "https://mail.google.com/mail/u/me@gmail.com/#all/11bdfc5cae0c8191"
        );
    }

    #[test]
    fn tls_config_builds() {
        assert!(tls_connector().is_ok());
        assert!(tls_connector().is_ok());
    }

    #[test]
    fn uidvalidity_change_clears_cache() {
        let mut cache = Cache::default();
        cache.validate(Some(1));
        cache.messages.insert(5, Message {
            id: "5".into(),
            from: String::new(),
            subject: String::new(),
            received: 0,
            unread: true,
            new: false,
            url: None,
        });
        cache.validate(Some(1));
        assert_eq!(cache.messages.len(), 1);
        cache.validate(Some(2));
        assert!(cache.messages.is_empty());
    }
}
