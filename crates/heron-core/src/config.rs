//! `~/.config/heron/config.toml`
//!
//! ```toml
//! [google_client]               # Desktop OAuth client from Google Cloud
//! client_id = "....apps.googleusercontent.com"
//! client_secret = "..."
//!
//! [[google]]
//! id = "personal"
//! email = "me@gmail.com"
//! # calendars = ["primary"]     # default: every calendar shown in Google Calendar
//!
//! [[proton]]
//! id = "proton"
//! username = "me@proton.me"
//!
//! [discord]
//! guild_id = "123456789012345678"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub proton: Vec<ProtonAccount>,
    #[serde(default)]
    pub google: Vec<GoogleAccount>,
    pub google_client: Option<GoogleClient>,
    pub discord: Option<DiscordConfig>,
    #[serde(default)]
    pub mail: MailSettings,
    #[serde(default)]
    pub calendar: CalendarSettings,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtonAccount {
    /// Account id used in the snapshot, CLI and credential file names.
    pub id: String,
    pub username: String,
    #[serde(default = "yes")]
    pub mail: bool,
    #[serde(default = "yes")]
    pub calendar: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleAccount {
    pub id: String,
    /// Expected address; `heron auth google` refuses a login as anyone else.
    pub email: String,
    #[serde(default = "yes")]
    pub mail: bool,
    #[serde(default = "yes")]
    pub calendar: bool,
    /// Calendar ids to include. `None` means every selected calendar.
    pub calendars: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    pub guild_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MailSettings {
    /// Unread messages listed per account.
    pub max_messages: usize,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self { max_messages: 50 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CalendarSettings {
    /// How far ahead the agenda reaches.
    pub days_ahead: u32,
    /// Poll interval for calendars without push.
    pub poll_secs: u64,
    /// Events lasting longer than this many days (project phases, trips) are
    /// shown only during their first day, not for as long as they run.
    /// 0 shows them throughout.
    pub long_event_days: u32,
}

impl Default for CalendarSettings {
    fn default() -> Self {
        Self { days_ahead: 7, poll_secs: 300, long_event_days: 7 }
    }
}

/// Points at `heron init` when there is no config file yet.
fn init_hint() -> &'static str {
    if paths::config_file().exists() { "" } else { " (no config yet: run `heron init`)" }
}

fn yes() -> bool {
    true
}

impl Config {
    /// Reads the config file. A missing file is an empty config.
    pub fn load() -> Result<Self> {
        let path = paths::config_file();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let config: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        let ids = self.proton.iter().map(|a| &a.id).chain(self.google.iter().map(|a| &a.id));
        for id in ids {
            anyhow::ensure!(
                !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "account id {id:?} must be letters, digits, '-' or '_'"
            );
            anyhow::ensure!(seen.insert(id), "account id {id:?} is used twice");
            anyhow::ensure!(id != "discord", "account id \"discord\" is reserved");
        }
        Ok(())
    }

    pub fn google_account(&self, id: &str) -> Result<&GoogleAccount> {
        self.google.iter().find(|a| a.id == id)
            .with_context(|| format!("no [[google]] account with id {id:?} in {}{}", paths::config_file().display(), init_hint()))
    }

    pub fn proton_account(&self, id: &str) -> Result<&ProtonAccount> {
        self.proton.iter().find(|a| a.id == id)
            .with_context(|| format!("no [[proton]] account with id {id:?} in {}{}", paths::config_file().display(), init_hint()))
    }
}
