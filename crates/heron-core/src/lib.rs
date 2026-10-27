//! Shared by `herond`, the `heron` CLI and the provider crates: the snapshot
//! the daemon publishes, the socket protocol, configuration, on-disk paths and
//! the [`Hub`] providers report into.

pub mod config;
pub mod hub;
pub mod mailbody;
pub mod meeting;
pub mod paths;
pub mod secrets;

use serde::{Deserialize, Serialize};

pub use config::Config;
pub use hub::{DiscordCommand, Hub, MailCommand, MailUpdate};
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
    /// Short name for the bar: the config's `label`, else the id.
    pub label: String,
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
    /// Computed by the hub from acknowledgements and per-message seen/unseen
    /// marks; providers leave it false.
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
    /// Video call link (Meet, Zoom, Teams, ...), copied when the event is clicked.
    #[serde(default)]
    pub meeting_url: Option<String>,
    /// Only sent by `Request::Event`; the hub strips it from snapshots to
    /// keep them small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<EventDetails>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EventDetails {
    /// Plain text, trimmed.
    pub description: Option<String>,
    pub organizer: Option<String>,
    /// Names or addresses, organizer excluded.
    pub attendees: Vec<String>,
}

/// A message's content, fetched on demand by `Request::Message`. Fetching
/// never marks the message read.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MessageBody {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// Unix seconds.
    pub date: i64,
    pub subject: String,
    /// Plain text: the text/plain part, or the HTML part converted to text.
    pub text: String,
    /// Links found in the message, in order, without duplicates.
    pub links: Vec<Link>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Link {
    /// The anchor text, or the URL itself when there is none.
    pub text: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordGuild {
    pub id: String,
    pub name: String,
    pub unread_channels: u32,
    /// Unread messages across visible, unmuted channels (each channel counts
    /// at most [`DISCORD_UNREAD_CAP`]).
    #[serde(default)]
    pub unread_messages: u32,
    /// Unread messages that notify the user: direct @mentions, replies that
    /// ping them, @everyone/@here and role mentions (when not suppressed).
    pub mentions: u32,
    /// Unread channels only, most mentions first.
    pub channels: Vec<DiscordChannel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordChannel {
    pub id: String,
    pub name: String,
    /// For a thread, the name of the channel it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Unread messages, at most [`DISCORD_UNREAD_CAP`].
    #[serde(default)]
    pub unread: u32,
    /// True when there are more unread messages than `unread` says.
    #[serde(default)]
    pub more: bool,
    pub mentions: u32,
    pub url: String,
    /// The newest unread messages, oldest first, at most
    /// [`DISCORD_PREVIEW_MESSAGES`].
    #[serde(default)]
    pub messages: Vec<DiscordMessage>,
}

/// Unread messages counted per channel before reporting `more`.
pub const DISCORD_UNREAD_CAP: u32 = 100;
/// Messages previewed per unread channel.
pub const DISCORD_PREVIEW_MESSAGES: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordMessage {
    pub id: String,
    /// Display name.
    pub author: String,
    /// Plain text, at most 200 characters; mentions resolved to names,
    /// attachments as "[image]" and similar.
    pub content: String,
    /// Unix seconds.
    pub timestamp: i64,
    /// This message notifies the user (mention, reply ping, @everyone, role).
    pub mentions_me: bool,
    /// Rich embeds, as bots such as alerting send them, at most
    /// [`DISCORD_PREVIEW_EMBEDS`]. Link previews are left out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embeds: Vec<DiscordEmbed>,
}

/// Embeds kept per previewed message.
pub const DISCORD_PREVIEW_EMBEDS: usize = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DiscordEmbed {
    pub title: Option<String>,
    /// Description, then one "name: value" line per field; line breaks kept,
    /// markdown and mentions reduced to plain text, at most 400 characters.
    pub text: String,
    /// Hex color of the embed's side bar, such as "#e74c3c".
    pub color: Option<String>,
    pub url: Option<String>,
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
    /// Clear "new" for one message, leaving it unread.
    MarkSeen { account: String, id: String },
    /// Make one message "new" again, until marked seen.
    MarkUnseen { account: String, id: String },
    /// Move one message to the account's trash.
    Trash { account: String, id: String },
    /// Fetch one message's content without marking it read.
    Message { account: String, id: String },
    /// One calendar event with its details.
    Event { id: String },
    /// Mark one Discord channel read up to its newest message, or every
    /// listed channel when `channel` is `None`.
    DiscordMarkRead { channel: Option<String> },
    /// Re-read the config and restart every provider.
    Reload,
}

/// One response per line from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Snapshot(Snapshot),
    Message(MessageBody),
    Event(CalendarEvent),
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
