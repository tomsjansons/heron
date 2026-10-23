//! Inbox unread list and message content. Message metadata is not
//! end-to-end encrypted, so listing needs no keys; changes are noticed through
//! the core event loop and any relevant event triggers a re-fetch of the list.
//!
//! Fetching one message (`GET /mail/v4/messages/{id}`, as go-proton-api's
//! `GetMessage`) returns its body as an armored PGP message encrypted to the
//! keys of the receiving address (`AddressID`). The read state is untouched:
//! Bridge fetches the same route for IMAP `BODY.PEEK[]` and marks messages
//! read with a separate `PUT /mail/v4/messages/read`.

use std::convert::Infallible;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use heron_core::hub::MailCommand;
use heron_core::{Hub, MailUpdate, Message, MessageBody, Provider, SourceKind, SourceState, mailbody};
use proton_crypto_account::proton_crypto::crypto::{DataEncoding, Decryptor, DecryptorSync, PGPProviderSync, VerifiedData};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::Backoff;
use crate::api::{Api, NeedsAuth, flag, needs_auth, or_default};
use crate::keys::Keyring;

/// Label id of the inbox.
const INBOX: &str = "0";
/// System label for Trash (go-proton-api `TrashLabel`).
const TRASH: &str = "3";
/// Largest page the messages route serves (go-proton-api `maxPageSize`).
const MAX_PAGE: usize = 150;
const EVENT_POLL: Duration = Duration::from_secs(25);
/// A message to an address the keyring lacks re-unlocks the keys at most this often.
const KEYRING_REFRESH: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ApiMessage {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub subject: String,
    pub sender: Option<Sender>,
    /// Older responses carry the sender flat instead of as `Sender`.
    pub sender_address: Option<String>,
    pub sender_name: Option<String>,
    #[serde(default, deserialize_with = "or_default")]
    pub time: i64,
    #[serde(default, deserialize_with = "flag")]
    pub unread: bool,
    #[serde(rename = "LabelIDs", default, deserialize_with = "or_default")]
    pub label_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Sender {
    #[serde(default, deserialize_with = "or_default")]
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub address: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Count {
    #[serde(rename = "LabelID")]
    pub label_id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub unread: u32,
}

impl Sender {
    fn display(&self) -> Option<String> {
        mailbody::format_address(Some(&self.name), Some(&self.address))
    }
}

/// One message with its encrypted body (go-proton-api `Message`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FullMessage {
    #[serde(rename = "AddressID")]
    pub address_id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub subject: String,
    pub sender: Option<Sender>,
    #[serde(default, deserialize_with = "or_default")]
    pub to_list: Vec<Sender>,
    #[serde(rename = "CCList", default, deserialize_with = "or_default")]
    pub cc_list: Vec<Sender>,
    #[serde(default, deserialize_with = "or_default")]
    pub time: i64,
    /// Armored PGP message.
    pub body: String,
    /// "text/html", "text/plain", or "multipart/mixed" for PGP/MIME.
    #[serde(rename = "MIMEType", default, deserialize_with = "or_default")]
    pub mime_type: String,
}

impl FullMessage {
    /// Decrypts and renders the body; headers come from the metadata.
    pub fn into_body<P: PGPProviderSync>(self, provider: &P, keys: &[impl AsRef<P::PrivateKey>], id: &str) -> Result<MessageBody> {
        // The signature is not verified; heron only previews.
        let plain = provider.new_decryptor()
            .with_decryption_key_refs(keys)
            .decrypt(&self.body, DataEncoding::Armor)
            .context("decrypting the message body")?;
        let plain = plain.as_bytes();
        let mime = self.mime_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
        let rendered = match mime.as_str() {
            "text/html" => mailbody::from_html(&String::from_utf8_lossy(plain)),
            "multipart/mixed" => mailbody::from_mime(plain).context("parsing the PGP/MIME body")?,
            _ => mailbody::from_text(&String::from_utf8_lossy(plain)),
        };
        let list = |list: &[Sender]| list.iter().filter_map(Sender::display).collect();
        Ok(MessageBody {
            id: id.to_owned(),
            from: self.sender.as_ref().and_then(Sender::display).unwrap_or_default(),
            to: list(&self.to_list),
            cc: list(&self.cc_list),
            date: self.time,
            subject: self.subject,
            text: rendered.text,
            links: rendered.links,
        })
    }
}

/// Address keys, unlocked on the first fetch and kept for the connection.
struct Keys<P: PGPProviderSync> {
    keyring: Option<(Keyring<P>, Instant)>,
}

impl<P: PGPProviderSync> Keys<P> {
    async fn for_address(&mut self, provider: &P, api: &Api, address_id: &str) -> Result<&[impl AsRef<P::PrivateKey> + use<P>]> {
        let stale = match &self.keyring {
            None => true,
            Some((keyring, at)) => !keyring.addresses.contains_key(address_id) && at.elapsed() >= KEYRING_REFRESH,
        };
        if stale {
            let passphrase = api.session().await.and_then(|s| s.key_passphrase)
                .ok_or_else(|| NeedsAuth("the stored session has no key passphrase".into()))?;
            self.keyring = Some((Keyring::unlock(provider, api, &passphrase).await?, Instant::now()));
        }
        let (keyring, _) = self.keyring.as_ref().expect("unlocked above");
        keyring.addresses.get(address_id).map(Vec::as_slice)
            .context("no key of the receiving address could be unlocked")
    }
}

async fn fetch_body<P: PGPProviderSync>(provider: &P, api: &Api, keys: &mut Keys<P>, id: &str) -> Result<MessageBody> {
    #[derive(Deserialize)]
    struct Response {
        #[serde(rename = "Message")]
        message: FullMessage,
    }
    anyhow::ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'=')),
        "not a Proton message id: {id:?}"
    );
    let message = api.get::<Response>(&format!("/mail/v4/messages/{id}"), &[]).await?.message;
    debug!("proton: fetched message {id} ({}, {} bytes armored)", message.mime_type, message.body.len());
    let keys = keys.for_address(provider, api, &message.address_id).await?;
    message.into_body(provider, keys, id)
}

impl ApiMessage {
    fn into_message(self) -> Message {
        let (name, address) = match self.sender {
            Some(sender) => (sender.name, sender.address),
            None => (self.sender_name.unwrap_or_default(), self.sender_address.unwrap_or_default()),
        };
        let from = if name.trim().is_empty() { address } else { name };
        Message {
            url: Some(format!("https://mail.proton.me/u/0/inbox/{}", self.id)),
            id: self.id,
            from,
            subject: self.subject,
            received: self.time,
            unread: self.unread,
            new: false,
        }
    }
}

/// Newest unread inbox messages first, capped at `max`, with the inbox unread
/// count from the counts route.
pub fn mail_update(messages: Vec<ApiMessage>, counts: &[Count], max: usize) -> MailUpdate {
    let mut messages: Vec<ApiMessage> = messages.into_iter()
        .filter(|m| m.unread && (m.label_ids.is_empty() || m.label_ids.iter().any(|l| l == INBOX)))
        .collect();
    messages.sort_by(|a, b| b.time.cmp(&a.time).then_with(|| a.id.cmp(&b.id)));
    messages.truncate(max);
    let listed = messages.len() as u32;
    let unread = counts.iter().find(|c| c.label_id == INBOX).map_or(listed, |c| c.unread.max(listed));
    MailUpdate { unread, messages: messages.into_iter().map(ApiMessage::into_message).collect() }
}

async fn fetch(api: &Api, max: usize) -> Result<MailUpdate> {
    #[derive(Deserialize)]
    struct Messages {
        #[serde(rename = "Messages", default)]
        messages: Vec<ApiMessage>,
    }
    #[derive(Deserialize)]
    struct Counts {
        #[serde(rename = "Counts", default)]
        counts: Vec<Count>,
    }

    let counts = api.get::<Counts>("/mail/v4/messages/count", &[]).await?.counts;
    let messages = if max == 0 {
        Vec::new()
    } else {
        // Parameters as in WebClients `queryMessageMetadata`.
        let query = [
            ("LabelID", INBOX.to_owned()),
            ("Unread", "1".to_owned()),
            ("Sort", "Time".to_owned()),
            ("Desc", "1".to_owned()),
            ("Page", "0".to_owned()),
            ("PageSize", max.min(MAX_PAGE).to_string()),
        ];
        api.get::<Messages>("/mail/v4/messages", &query).await?.messages
    };
    Ok(mail_update(messages, &counts, max))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Event {
    #[serde(rename = "EventID")]
    event_id: String,
    #[serde(default, deserialize_with = "flag")]
    more: bool,
    #[serde(default)]
    refresh: u32,
    #[serde(default, deserialize_with = "or_default")]
    messages: Vec<IgnoredAny>,
    message_counts: Option<IgnoredAny>,
}

async fn latest_event(api: &Api) -> Result<String> {
    Ok(api.get::<Event>("/core/v4/events/latest", &[]).await?.event_id)
}

/// Follows the event loop from `cursor`; returns whether mail changed.
async fn poll_events(api: &Api, cursor: &mut String) -> Result<bool> {
    let mut changed = false;
    loop {
        let event: Event = api.get(&format!("/core/v4/events/{cursor}"), &[]).await?;
        if event.refresh != 0 {
            // The cursor is no longer usable; start over from the latest event.
            *cursor = latest_event(api).await?;
            return Ok(true);
        }
        changed |= !event.messages.is_empty() || event.message_counts.is_some();
        *cursor = event.event_id;
        if !event.more {
            return Ok(changed);
        }
    }
}

async fn apply(api: &Api, command: &MailCommand) -> Result<()> {
    let (route, id) = match command {
        MailCommand::MarkRead { id, .. } => ("/mail/v4/messages/read", id),
        MailCommand::MarkUnread { id, .. } => ("/mail/v4/messages/unread", id),
        MailCommand::Trash { id, .. } => {
            // Trashing is labelling with the system Trash label.
            let body = json!({ "LabelID": TRASH, "IDs": [id] });
            return api.put::<IgnoredAny>("/mail/v4/messages/label", &body).await.map(drop);
        }
        MailCommand::Fetch { .. } => anyhow::bail!("fetch is not a flag change"),
    };
    api.put::<IgnoredAny>(route, &json!({ "IDs": [id] })).await.map(drop)
}

fn reply(command: MailCommand, result: Result<()>) {
    match (command, result) {
        (MailCommand::MarkRead { reply, .. } | MailCommand::MarkUnread { reply, .. } | MailCommand::Trash { reply, .. }, result) => {
            let _ = reply.send(result);
        }
        (other, result) => other.fail(result.err().unwrap_or_else(|| anyhow::anyhow!("unexpected reply"))),
    }
}

/// Runs until the session needs a new login, which is returned as the error.
pub async fn run(api: &Api, account: &str, max: usize, hub: &Hub, commands: &mut mpsc::Receiver<MailCommand>) -> anyhow::Error {
    let provider = proton_crypto_account::proton_crypto::new_pgp_provider();
    let mut backoff = Backoff::default();
    loop {
        let Err(err) = connected(&provider, api, account, max, hub, commands, &mut backoff).await;
        if needs_auth(&err).is_some() {
            return err;
        }
        let delay = backoff.next();
        warn!("proton/{account} mail: {err:#}; retrying in {}s", delay.as_secs());
        hub.set_status(account, SourceKind::ProtonMail, SourceState::Error, Some(crate::short(&err)));

        let sleep = tokio::time::sleep(delay);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                Some(command) = commands.recv() => reply(command, Err(anyhow::anyhow!("Proton Mail is reconnecting"))),
            }
        }
    }
}

async fn connected<P: PGPProviderSync>(
    provider: &P,
    api: &Api,
    account: &str,
    max: usize,
    hub: &Hub,
    commands: &mut mpsc::Receiver<MailCommand>,
    backoff: &mut Backoff,
) -> Result<Infallible> {
    let mut cursor = latest_event(api).await?;
    let publish = |update: MailUpdate| {
        hub.set_mail(account, Provider::Proton, update);
        hub.set_status(account, SourceKind::ProtonMail, SourceState::Ok, None);
    };
    publish(fetch(api, max).await?);
    backoff.reset();
    let mut keys = Keys { keyring: None };

    let mut ticker = tokio::time::interval(EVENT_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if poll_events(api, &mut cursor).await? {
                    debug!("proton/{account}: mail changed");
                    publish(fetch(api, max).await?);
                }
            }
            Some(command) = commands.recv() => {
                if let MailCommand::Fetch { id, reply } = command {
                    // Read state is untouched, so the list needs no refresh.
                    match fetch_body(provider, api, &mut keys, &id).await {
                        Ok(body) => drop(reply.send(Ok(body))),
                        Err(err) => {
                            warn!("proton/{account}: fetching message {id}: {err:#}");
                            let _ = reply.send(Err(anyhow::anyhow!("{err:#}")));
                            if needs_auth(&err).is_some() {
                                return Err(err);
                            }
                        }
                    }
                    continue;
                }
                let result = apply(api, &command).await;
                let failed = result.as_ref().err().map(|e| format!("{e:#}"));
                reply(command, result);
                if let Some(err) = failed {
                    warn!("proton/{account}: {err}");
                }
                publish(fetch(api, max).await?);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGES: &str = r#"{
        "Code": 1000,
        "Total": 4,
        "Messages": [
            {"ID": "older", "Subject": "Older", "Sender": {"Name": "", "Address": "a@example.com"},
             "Time": 1700000000, "Unread": 1, "LabelIDs": ["0", "5"]},
            {"ID": "newest", "Subject": "Newest", "Sender": {"Name": "Bob", "Address": "bob@example.com"},
             "Time": 1700000300, "Unread": 1, "LabelIDs": ["0"]},
            {"ID": "read", "Subject": "Read", "Sender": {"Name": "C", "Address": "c@example.com"},
             "Time": 1700000200, "Unread": 0, "LabelIDs": ["0"]},
            {"ID": "archived", "Subject": "Archived", "SenderAddress": "d@example.com", "SenderName": "Dee",
             "Time": 1700000100, "Unread": 1, "LabelIDs": ["6"]}
        ]
    }"#;

    #[derive(Deserialize)]
    struct Fixture {
        #[serde(rename = "Messages")]
        messages: Vec<ApiMessage>,
    }

    #[test]
    fn maps_unread_inbox_newest_first() {
        let fixture: Fixture = serde_json::from_str(MESSAGES).unwrap();
        let counts: Vec<Count> = serde_json::from_str(r#"[{"LabelID":"0","Total":90,"Unread":12},{"LabelID":"5","Unread":1}]"#).unwrap();
        let update = mail_update(fixture.messages, &counts, 50);

        assert_eq!(update.unread, 12);
        let ids: Vec<&str> = update.messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["newest", "older"]);
        assert_eq!(update.messages[0].from, "Bob");
        assert_eq!(update.messages[1].from, "a@example.com");
        assert_eq!(update.messages[0].received, 1700000300);
        assert!(update.messages.iter().all(|m| m.unread && !m.new));
        assert_eq!(update.messages[0].url.as_deref(), Some("https://mail.proton.me/u/0/inbox/newest"));
    }

    #[test]
    fn caps_list_and_never_reports_fewer_unread_than_listed() {
        let fixture: Fixture = serde_json::from_str(MESSAGES).unwrap();
        let update = mail_update(fixture.messages, &[], 1);
        assert_eq!(update.messages.len(), 1);
        assert_eq!(update.unread, 1);
    }

    mod body {
        use proton_crypto_account::proton_crypto::crypto::{
            Encryptor, EncryptorSync, KeyGenerator, KeyGeneratorSync, PGPProviderSync,
        };
        use proton_crypto_account::proton_crypto::new_pgp_provider;

        use super::*;

        /// Encrypts like a Proton client: to the recipient address key, signed by the sender.
        fn encrypt<P: PGPProviderSync>(provider: &P, recipient: &P::PrivateKey, plain: &str) -> String {
            let sender = provider.new_key_generator().with_user_id("Bob", "bob@example.com").generate().unwrap();
            let public = provider.private_key_to_public_key(recipient).unwrap();
            let armored = provider.new_encryptor()
                .with_encryption_key(&public)
                .with_signing_key(&sender)
                .encrypt_raw(plain, DataEncoding::Armor)
                .unwrap();
            String::from_utf8(armored).unwrap()
        }

        fn message(mime: &str, armored: &str) -> FullMessage {
            serde_json::from_value(json!({
                "ID": "m1", "AddressID": "addr1", "Subject": "Quarterly report",
                "Sender": {"Name": "Bob", "Address": "bob@example.com"},
                "ToList": [{"Name": "", "Address": "me@proton.me"}, {"Name": "Anna", "Address": "anna@example.com"}],
                "CCList": [{"Name": "Team", "Address": "team@example.com"}],
                "BCCList": [], "Time": 1700000000, "Unread": 1, "Flags": 1,
                "Header": "From: ignored@example.com\r\n", "ParsedHeaders": {},
                "Body": armored, "MIMEType": mime, "Attachments": []
            }))
            .unwrap()
        }

        #[test]
        fn decrypts_html_body() {
            let provider = new_pgp_provider();
            let key = provider.new_key_generator().with_user_id("Me", "me@proton.me").generate().unwrap();
            let html = "<html><body><p>Hi,</p><p>The <a href=\"https://example.com/q3\">Q3 report</a> is ready.</p>\
                        <img src=\"https://t.example/p.gif\" width=\"1\" height=\"1\"></body></html>";
            let body = message("text/html", &encrypt(&provider, &key, html)).into_body(&provider, &[key], "m1").unwrap();
            assert_eq!(body.id, "m1");
            assert_eq!(body.from, "Bob <bob@example.com>");
            assert_eq!(body.to, ["me@proton.me", "Anna <anna@example.com>"]);
            assert_eq!(body.cc, ["Team <team@example.com>"]);
            assert_eq!(body.subject, "Quarterly report");
            assert_eq!(body.date, 1_700_000_000);
            assert_eq!(body.text, "Hi,\n\nThe Q3 report is ready.");
            assert_eq!(body.links, [heron_core::Link { text: "Q3 report".into(), url: "https://example.com/q3".into() }]);
        }

        #[test]
        fn decrypts_plain_body() {
            let provider = new_pgp_provider();
            let key = provider.new_key_generator().with_user_id("Me", "me@proton.me").generate().unwrap();
            let armored = encrypt(&provider, &key, "Rīga, café — see https://example.lv/x.\r\n\r\n\r\n\r\n-- \r\nBob");
            let body = message("text/plain", &armored).into_body(&provider, &[key], "m1").unwrap();
            assert_eq!(body.text, "Rīga, café — see https://example.lv/x.\n\n--\nBob");
            assert_eq!(body.links.len(), 1);
            assert_eq!(body.links[0].url, "https://example.lv/x");
        }

        #[test]
        fn decrypts_pgp_mime_body() {
            let provider = new_pgp_provider();
            let key = provider.new_key_generator().with_user_id("Me", "me@proton.me").generate().unwrap();
            let mime = "Content-Type: multipart/mixed; boundary=\"x\"\r\n\r\n--x\r\n\
                Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n\
                Sveiki, R=C4=ABga! Details: https://example.lv/d\r\n--x\r\n\
                Content-Type: application/octet-stream; name=a.bin\r\nContent-Disposition: attachment\r\n\r\nAAAA\r\n--x--\r\n";
            let body = message("multipart/mixed", &encrypt(&provider, &key, mime)).into_body(&provider, &[key], "m1").unwrap();
            assert_eq!(body.text, "Sveiki, Rīga! Details: https://example.lv/d");
            assert_eq!(body.from, "Bob <bob@example.com>", "headers come from the metadata");
            assert_eq!(body.links.len(), 1);
        }

        #[test]
        fn wrong_key_fails() {
            let provider = new_pgp_provider();
            let key = provider.new_key_generator().with_user_id("Me", "me@proton.me").generate().unwrap();
            let other = provider.new_key_generator().with_user_id("Other", "o@proton.me").generate().unwrap();
            let err = message("text/html", &encrypt(&provider, &key, "<p>x</p>")).into_body(&provider, &[other], "m1").unwrap_err();
            assert!(format!("{err:#}").contains("decrypting the message body"));
        }
    }

    #[test]
    fn event_detects_changes() {
        let event: Event = serde_json::from_str(r#"{"Code":1000,"EventID":"e2","More":0,"Refresh":0,
            "Messages":[{"ID":"m","Action":3,"Message":{"ID":"m","Unread":0}}]}"#).unwrap();
        assert_eq!(event.event_id, "e2");
        assert!(!event.more && event.refresh == 0 && event.messages.len() == 1);
        let quiet: Event = serde_json::from_str(r#"{"Code":1000,"EventID":"e3","More":1}"#).unwrap();
        assert!(quiet.more && quiet.messages.is_empty() && quiet.message_counts.is_none());
    }
}
