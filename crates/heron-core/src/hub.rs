//! The hub collects what each provider reports and publishes one [`Snapshot`].
//!
//! Providers own their connections and push whole values: the current unread
//! list of an account, every upcoming event of a calendar source, the Discord
//! guild state. The hub decides what is "new", merges, filters and publishes.
//! Commands that change server state (mark read) are routed back to the mail
//! provider that registered the account.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::warn;

use crate::{
    CalendarEvent, DiscordGuild, MailAccount, Message, MessageBody, Provider, Snapshot, SourceKind, SourceState,
    SourceStatus, config::CalendarSettings, now, paths,
};

/// What a mail provider knows about one account.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MailUpdate {
    /// Unread count on the server, which may exceed `messages.len()`.
    pub unread: u32,
    /// Newest unread messages first, capped at `mail.max_messages`.
    pub messages: Vec<Message>,
}

/// Sent to the provider that registered the account. Reply when the server
/// has accepted the change; the provider then publishes a fresh [`MailUpdate`].
#[derive(Debug)]
pub enum MailCommand {
    MarkRead { id: String, reply: oneshot::Sender<Result<()>> },
    MarkUnread { id: String, reply: oneshot::Sender<Result<()>> },
    /// Fetch the content without changing the message's read state.
    Fetch { id: String, reply: oneshot::Sender<Result<MessageBody>> },
    /// Move the message to the account's trash.
    Trash { id: String, reply: oneshot::Sender<Result<()>> },
}

impl MailCommand {
    /// Answers with an error, whatever the command.
    pub fn fail(self, err: anyhow::Error) {
        match self {
            Self::MarkRead { reply, .. } | Self::MarkUnread { reply, .. } | Self::Trash { reply, .. } => {
                let _ = reply.send(Err(err));
            }
            Self::Fetch { reply, .. } => {
                let _ = reply.send(Err(err));
            }
        }
    }
}

/// What the shell has seen, persisted in `seen.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Seen {
    /// Per account: messages received at or before this unix time are not new.
    acks: BTreeMap<String, i64>,
    /// Per account: messages marked seen individually.
    seen: BTreeMap<String, BTreeSet<String>>,
    /// Per account: messages marked unseen, which stay new until marked seen.
    unseen: BTreeMap<String, BTreeSet<String>>,
}

impl Seen {
    fn is_new(&self, account: &str, message: &Message) -> bool {
        let has = |map: &BTreeMap<String, BTreeSet<String>>| map.get(account).is_some_and(|ids| ids.contains(&message.id));
        if has(&self.unseen) {
            return true;
        }
        let ack = self.acks.get(account).copied().unwrap_or(i64::MAX);
        message.unread && message.received > ack && !has(&self.seen)
    }
}

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Mutex<Inner>>,
    tx: Arc<watch::Sender<Snapshot>>,
}

struct Inner {
    calendar: CalendarSettings,
    mail: BTreeMap<String, (Provider, MailUpdate)>,
    calendars: BTreeMap<String, Vec<CalendarEvent>>,
    discord: Option<DiscordGuild>,
    status: BTreeMap<String, SourceStatus>,
    seen: Seen,
    labels: BTreeMap<String, String>,
    mail_commands: HashMap<String, mpsc::Sender<MailCommand>>,
}

impl Hub {
    pub fn new(calendar: &CalendarSettings) -> Self {
        let inner = Inner {
            calendar: calendar.clone(),
            mail: BTreeMap::new(),
            calendars: BTreeMap::new(),
            discord: None,
            status: BTreeMap::new(),
            seen: load_seen(),
            labels: BTreeMap::new(),
            mail_commands: HashMap::new(),
        };
        let (tx, _) = watch::channel(Snapshot::default());
        Self { inner: Arc::new(Mutex::new(inner)), tx: Arc::new(tx) }
    }

    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.tx.subscribe()
    }

    /// Forgets every source, keeping acknowledgements. Used on reload.
    pub fn reset(&self, calendar: &CalendarSettings) {
        let mut inner = self.lock();
        inner.calendar = calendar.clone();
        inner.mail.clear();
        inner.calendars.clear();
        inner.discord = None;
        inner.status.clear();
        inner.mail_commands.clear();
        self.publish(&inner);
    }

    /// Account id → short name for the bar.
    pub fn set_labels(&self, labels: BTreeMap<String, String>) {
        let mut inner = self.lock();
        inner.labels = labels;
        self.publish(&inner);
    }

    pub fn set_status(&self, id: &str, kind: SourceKind, state: SourceState, message: Option<String>) {
        let mut inner = self.lock();
        let status = SourceStatus { id: id.to_owned(), kind, state, message };
        inner.status.insert(format!("{id}/{kind:?}"), status);
        self.publish(&inner);
    }

    pub fn set_mail(&self, account: &str, provider: Provider, update: MailUpdate) {
        let mut inner = self.lock();
        // The first time an account is seen, what is already unread is not new.
        let mut changed = false;
        if !inner.seen.acks.contains_key(account) {
            inner.seen.acks.insert(account.to_owned(), now());
            changed = true;
        }
        // Forget marks on messages that left the list.
        let listed: BTreeSet<&str> = update.messages.iter().map(|m| m.id.as_str()).collect();
        let state = &mut inner.seen;
        for map in [&mut state.seen, &mut state.unseen] {
            if let Some(ids) = map.get_mut(account) {
                let before = ids.len();
                ids.retain(|id| listed.contains(id.as_str()));
                changed |= ids.len() != before;
            }
        }
        if changed {
            save_seen(&inner.seen);
        }
        inner.mail.insert(account.to_owned(), (provider, update));
        self.publish(&inner);
    }

    /// Replaces every event from `source`. Pass occurrences, not recurrence rules.
    pub fn set_calendar(&self, source: &str, events: Vec<CalendarEvent>) {
        let mut inner = self.lock();
        inner.calendars.insert(source.to_owned(), events);
        self.publish(&inner);
    }

    pub fn set_discord(&self, guild: Option<DiscordGuild>) {
        let mut inner = self.lock();
        inner.discord = guild;
        self.publish(&inner);
    }

    /// Called by a mail provider once per account; commands arrive on the receiver.
    pub fn register_mail(&self, account: &str) -> mpsc::Receiver<MailCommand> {
        let (tx, rx) = mpsc::channel(16);
        self.lock().mail_commands.insert(account.to_owned(), tx);
        rx
    }

    pub async fn mark_read(&self, account: &str, id: &str) -> Result<()> {
        self.mail_command(account, |reply| MailCommand::MarkRead { id: id.to_owned(), reply }).await
    }

    pub async fn mark_unread(&self, account: &str, id: &str) -> Result<()> {
        self.mail_command(account, |reply| MailCommand::MarkUnread { id: id.to_owned(), reply }).await
    }

    /// Moves one message to the account's trash.
    pub async fn trash(&self, account: &str, id: &str) -> Result<()> {
        self.mail_command(account, |reply| MailCommand::Trash { id: id.to_owned(), reply }).await
    }

    /// Fetches one message's content from its provider.
    pub async fn fetch_message(&self, account: &str, id: &str) -> Result<MessageBody> {
        self.mail_command(account, |reply| MailCommand::Fetch { id: id.to_owned(), reply }).await
    }

    /// One upcoming event, with details, by id.
    pub fn event(&self, id: &str) -> Option<CalendarEvent> {
        self.lock().calendars.values().flatten().find(|e| e.id == id).cloned()
    }

    /// Clears "new" for one message.
    pub fn mark_seen(&self, account: &str, id: &str) -> Result<()> {
        self.mark(account, id, true)
    }

    /// Makes one message "new" until it is marked seen.
    pub fn mark_unseen(&self, account: &str, id: &str) -> Result<()> {
        self.mark(account, id, false)
    }

    fn mark(&self, account: &str, id: &str, seen: bool) -> Result<()> {
        let mut inner = self.lock();
        anyhow::ensure!(inner.mail.contains_key(account), "no mail account {account:?}");
        let state = &mut inner.seen;
        let (add, remove) = if seen { (&mut state.seen, &mut state.unseen) } else { (&mut state.unseen, &mut state.seen) };
        if let Some(ids) = remove.get_mut(account) {
            ids.remove(id);
        }
        add.entry(account.to_owned()).or_default().insert(id.to_owned());
        save_seen(&inner.seen);
        self.publish(&inner);
        Ok(())
    }

    async fn mail_command<T>(
        &self,
        account: &str,
        build: impl FnOnce(oneshot::Sender<Result<T>>) -> MailCommand,
    ) -> Result<T> {
        let sender = self.lock().mail_commands.get(account).cloned()
            .with_context(|| format!("no mail account {account:?} is running"))?;
        let (reply, response) = oneshot::channel();
        sender.send(build(reply)).await.context("mail provider stopped")?;
        tokio::time::timeout(Duration::from_secs(30), response).await
            .context("mail provider did not answer in 30s")?
            .context("mail provider dropped the request")?
    }

    /// Clears "new" for one account, or every account when `None`. Messages
    /// marked unseen stay new.
    pub fn ack(&self, account: Option<&str>) -> Result<()> {
        let mut inner = self.lock();
        let accounts: Vec<String> = match account {
            Some(id) => {
                anyhow::ensure!(inner.mail.contains_key(id), "no mail account {id:?}");
                vec![id.to_owned()]
            }
            None => inner.mail.keys().cloned().collect(),
        };
        let at = now();
        for id in accounts {
            inner.seen.seen.remove(&id);
            inner.seen.acks.insert(id, at);
        }
        save_seen(&inner.seen);
        self.publish(&inner);
        Ok(())
    }

    /// Republishes, so events that have ended drop out of the agenda.
    pub fn tick(&self) {
        let inner = self.lock();
        self.publish(&inner);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(&self, inner: &Inner) {
        let snapshot = inner.snapshot(now());
        self.tx.send_if_modified(|current| {
            if *current == snapshot {
                return false;
            }
            *current = snapshot;
            true
        });
    }
}

impl Inner {
    fn snapshot(&self, now: i64) -> Snapshot {
        let mail: Vec<MailAccount> = self.mail.iter()
            .map(|(id, (provider, update))| {
                let messages: Vec<Message> = update.messages.iter()
                    .map(|m| Message { new: self.seen.is_new(id, m), ..m.clone() })
                    .collect();
                MailAccount {
                    id: id.clone(),
                    label: self.labels.get(id).cloned().unwrap_or_else(|| id.clone()),
                    provider: *provider,
                    new: messages.iter().filter(|m| m.new).count() as u32,
                    unread: update.unread,
                    messages,
                }
            })
            .collect();

        let horizon = now + i64::from(self.calendar.days_ahead) * 86_400;
        let long = i64::from(self.calendar.long_event_days) * 86_400;
        // A long event stays visible for the first day after it starts.
        let is_stale_long = |e: &CalendarEvent| long > 0 && e.end - e.start > long && e.start < now - 86_400;
        let mut calendar: Vec<CalendarEvent> = self.calendars.values()
            .flatten()
            .filter(|e| e.end > now && e.start < horizon && !is_stale_long(e))
            .map(|e| CalendarEvent { details: None, ..e.clone() })
            .collect();
        calendar.sort_by(|a, b| (a.start, &a.title).cmp(&(b.start, &b.title)));

        Snapshot {
            new: mail.iter().map(|a| a.new).sum(),
            unread: mail.iter().map(|a| a.unread).sum(),
            mail,
            calendar,
            discord: self.discord.clone(),
            sources: self.status.values().cloned().collect(),
        }
    }
}

fn seen_file() -> std::path::PathBuf {
    paths::state_dir().join("seen.json")
}

/// Reads `seen.json`, or the acknowledgements of the older `acks.json`.
fn load_seen() -> Seen {
    if let Ok(bytes) = std::fs::read(seen_file()) {
        return serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            warn!("ignoring unreadable seen.json: {err}");
            Seen::default()
        });
    }
    let acks = std::fs::read(paths::state_dir().join("acks.json")).ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    Seen { acks, ..Seen::default() }
}

fn save_seen(seen: &Seen) {
    let path = seen_file();
    let result = std::fs::create_dir_all(paths::state_dir())
        .and_then(|_| std::fs::write(&path, serde_json::to_vec_pretty(seen).unwrap_or_default()));
    if let Err(err) = result {
        warn!("writing {}: {err}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, received: i64, unread: bool) -> Message {
        Message {
            id: id.into(),
            from: "a@b".into(),
            subject: "s".into(),
            received,
            unread,
            new: false,
            url: None,
        }
    }

    #[test]
    fn new_is_unread_after_ack() {
        let mut inner = Inner {
            calendar: CalendarSettings::default(),
            mail: BTreeMap::new(),
            calendars: BTreeMap::new(),
            discord: None,
            status: BTreeMap::new(),
            seen: Seen { acks: BTreeMap::from([("p".to_owned(), 100)]), ..Seen::default() },
            labels: BTreeMap::new(),
            mail_commands: HashMap::new(),
        };
        let update = MailUpdate {
            unread: 5,
            messages: vec![message("old", 50, true), message("fresh", 150, true), message("read", 160, false)],
        };
        inner.mail.insert("p".into(), (Provider::Proton, update));

        let snapshot = inner.snapshot(200);
        assert_eq!(snapshot.new, 1);
        assert_eq!(snapshot.unread, 5);
        let flags: Vec<bool> = snapshot.mail[0].messages.iter().map(|m| m.new).collect();
        assert_eq!(flags, [false, true, false]);

        // Seen clears one message; unseen pins one as new even when old or read.
        inner.seen.seen.insert("p".into(), BTreeSet::from(["fresh".to_owned()]));
        inner.seen.unseen.insert("p".into(), BTreeSet::from(["old".to_owned(), "read".to_owned()]));
        let flags: Vec<bool> = inner.snapshot(200).mail[0].messages.iter().map(|m| m.new).collect();
        assert_eq!(flags, [true, false, true]);
    }

    #[test]
    fn calendar_window_and_order() {
        let event = |id: &str, start, end| CalendarEvent {
            id: id.into(),
            source: "g".into(),
            calendar: "c".into(),
            title: id.into(),
            location: None,
            start,
            end,
            all_day: false,
            color: None,
            url: None,
            meeting_url: None,
            details: None,
        };
        let inner = Inner {
            calendar: CalendarSettings { days_ahead: 1, ..CalendarSettings::default() },
            mail: BTreeMap::new(),
            calendars: BTreeMap::from([(
                "g".to_owned(),
                vec![
                    event("later", 2_000, 3_000),
                    event("ended", 0, 500),
                    event("now", 900, 1_100),
                    event("far", 1_000 + 86_400, 90_000),
                    // Longer than 7 days: hidden once it has run a day, shown on its first day.
                    event("long-stale", 1_000 - 2 * 86_400, 1_000 + 20 * 86_400),
                    event("long-fresh", 950, 1_000 + 20 * 86_400),
                ],
            )]),
            discord: None,
            status: BTreeMap::new(),
            seen: Seen::default(),
            labels: BTreeMap::new(),
            mail_commands: HashMap::new(),
        };
        let ids: Vec<String> = inner.snapshot(1_000).calendar.into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["now", "long-fresh", "later"]);
    }
}
