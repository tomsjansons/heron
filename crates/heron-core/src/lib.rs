//! Shared by `herond`, the `heron` CLI and the provider crates: the snapshot
//! the daemon publishes, the socket protocol, configuration, on-disk paths and
//! the [`Hub`] providers report into.

pub mod config;
pub mod hub;
pub mod paths;
pub mod secrets;

use serde::{Deserialize, Serialize};

pub use config::Config;
pub use hub::{Hub, MailCommand, MailUpdate};
pub use paths::socket_path;

/// Everything the shell needs to render, published as one value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    /// Totals across every mail account.
    pub new: u32,
    pub unread: u32,
    pub mail: Vec<MailAccount>,
    /// Upcoming events from every calendar, sorted by start.
    pub calendar: Vec<CalendarEvent>,
    pub discord: Option<DiscordGuild>,
    /// One entry per configured source, for "login required" and errors.
    pub sources: Vec<SourceStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailAccount {
    pub id: String,
    pub provider: Provider,
    /// Unread and received after the account's last acknowledgement.
    pub new: u32,
    /// Unread on the server, whatever the shell has seen.
    pub unread: u32,
    /// Most recent unread messages, newest first.
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Gmail,
    Proton,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    /// Stable within the account; passed back in mark-read and mark-unread.
    pub id: String,
    pub from: String,
    pub subject: String,
    /// Unix seconds.
    pub received: i64,
    pub unread: bool,
    /// Computed by the hub; providers leave it false.
    #[serde(default)]
    pub new: bool,
    /// Opens the message in the provider's web client.
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalendarEvent {
    /// Unique per occurrence, so recurring events expand to distinct ids.
    pub id: String,
    /// Source id from the config, such as the Google account id.
    pub source: String,
    /// Human name of the calendar.
    pub calendar: String,
    pub title: String,
    pub location: Option<String>,
    /// Unix seconds. All-day events start at local midnight.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    /// Hex color, such as "#7986cb".
    pub color: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordGuild {
    pub id: String,
    pub name: String,
    pub unread_channels: u32,
    pub mentions: u32,
    /// Unread channels only, most mentions first.
    pub channels: Vec<DiscordChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordChannel {
    pub id: String,
    pub name: String,
    pub mentions: u32,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceStatus {
    /// Matches an account id from the config, or "discord".
    pub id: String,
    pub kind: SourceKind,
    pub state: SourceState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    ProtonMail,
    ProtonCalendar,
    Gmail,
    GoogleCalendar,
    Discord,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceState {
    Connecting,
    Ok,
    /// No stored credentials, or they were revoked; run `heron auth`.
    NeedsAuth,
    /// Retrying after a failure; `message` says why.
    Error,
}

/// One request per line from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Reply with the current snapshot, then close.
    Status,
    /// Reply with the current snapshot, then one more line per change.
    Watch,
    /// Clear "new" for one account, or all of them.
    Ack { account: Option<String> },
    MarkRead { account: String, id: String },
    MarkUnread { account: String, id: String },
    /// Re-read the config and restart every provider.
    Reload,
}

/// One response per line from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Snapshot(Snapshot),
    Ok,
    Error { message: String },
}

/// Current time in unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
