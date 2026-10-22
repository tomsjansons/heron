//! Inbox unread list. Message metadata is not end-to-end encrypted, so no keys
//! are needed. Changes are noticed through the core event loop; any relevant
//! event triggers a re-fetch of the list.

use std::convert::Infallible;
use std::time::Duration;

use anyhow::Result;
use heron_core::hub::MailCommand;
use heron_core::{Hub, MailUpdate, Message, Provider, SourceKind, SourceState};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::Backoff;
use crate::api::{Api, flag, needs_auth, or_default};

/// Label id of the inbox.
const INBOX: &str = "0";
/// Largest page the messages route serves (go-proton-api `maxPageSize`).
const MAX_PAGE: usize = 150;
const EVENT_POLL: Duration = Duration::from_secs(25);

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
    };
    api.put::<IgnoredAny>(route, &json!({ "IDs": [id] })).await.map(drop)
}

fn reply(command: MailCommand, result: Result<()>) {
    let (MailCommand::MarkRead { reply, .. } | MailCommand::MarkUnread { reply, .. }) = command;
    let _ = reply.send(result);
}

/// Runs until the session needs a new login, which is returned as the error.
pub async fn run(api: &Api, account: &str, max: usize, hub: &Hub, commands: &mut mpsc::Receiver<MailCommand>) -> anyhow::Error {
    let mut backoff = Backoff::default();
    loop {
        let Err(err) = connected(api, account, max, hub, commands, &mut backoff).await;
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

async fn connected(
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
