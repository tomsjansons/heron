//! Types shared by `herond` and the `heron` CLI: the snapshot the daemon
//! publishes and the line-delimited JSON protocol spoken over its socket.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Everything the shell needs to render, published as one value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub mail: Vec<MailAccount>,
    pub calendar: Vec<CalendarEvent>,
    pub discord: Option<DiscordGuild>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MailAccount {
    pub id: String,
    pub provider: Provider,
    /// Unread and not yet acknowledged in the shell.
    pub new: u32,
    /// Unread on the server, whatever the shell has seen.
    pub unread: u32,
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
    /// Stable per account: Proton message id or Gmail `UIDVALIDITY:UID`.
    pub id: String,
    pub from: String,
    pub subject: String,
    /// Unix seconds.
    pub received: i64,
    pub unread: bool,
    pub new: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalendarEvent {
    pub id: String,
    pub calendar: String,
    pub title: String,
    pub location: Option<String>,
    /// Unix seconds.
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DiscordGuild {
    pub name: String,
    pub unread_channels: u32,
    pub mentions: u32,
}

/// One request per line from client to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Reply with the current snapshot, then close.
    Status,
    /// Reply with the current snapshot, then one more line per change.
    Watch,
    /// Mark everything currently unread as seen in the shell, clearing `new`.
    Ack { account: Option<String> },
    MarkRead { account: String, id: String },
    MarkUnread { account: String, id: String },
}

/// One response per line from daemon to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Snapshot(Snapshot),
    Ok,
    Error { message: String },
}

/// `$XDG_RUNTIME_DIR/heron.sock`, falling back to `/tmp/heron-$UID.sock`.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("heron.sock"),
        None => {
            let uid = std::env::var("UID").unwrap_or_else(|_| "user".into());
            PathBuf::from(format!("/tmp/heron-{uid}.sock"))
        }
    }
}
