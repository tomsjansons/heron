//! Gmail over IMAP with XOAUTH2, and Google Calendar through Calendar API v3.
//!
//! # Setting up Google access
//!
//! heron signs in with your own OAuth client, so you need a (free) Google
//! Cloud project once:
//!
//! 1. At <https://console.cloud.google.com/>, create a project.
//! 2. APIs & Services → Library: enable the **Gmail API** and the **Google
//!    Calendar API**.
//! 3. APIs & Services → OAuth consent screen (Google Auth Platform): user type
//!    *External*, any app name, your address as support and developer
//!    contact. Under *Audience*, add your Google address(es) as test users,
//!    or better, **publish the app** ("In production"): while it is in
//!    "Testing", refresh tokens expire after 7 days and you would have to sign
//!    in again weekly. An unverified app in production only shows a warning
//!    screen ("Google hasn't verified this app" → Advanced → Go to …).
//! 4. APIs & Services → Credentials → Create credentials → OAuth client ID →
//!    application type **Desktop app**. Copy the client id and secret into
//!    `~/.config/heron/config.toml`:
//!
//!    ```toml
//!    [google_client]
//!    client_id = "1234-abc.apps.googleusercontent.com"
//!    client_secret = "GOCSPX-..."
//!    ```
//! 5. Add a `[[google]]` account with `id` and `email`, then run
//!    `heron auth google <id>` and approve access in the browser. Tick every
//!    box: Gmail needs "Read, compose, send and permanently delete all your
//!    email" (the only scope IMAP accepts) and the agenda needs "See and
//!    download any calendar you can access".
//!
//! `heron logout google <id>` revokes the grant and deletes the stored token.

mod calendar;
mod gmail;
mod login;
mod oauth;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use heron_core::config::{Config, GoogleAccount, GoogleClient};
use heron_core::{Hub, SourceKind, SourceState, secrets};
use tracing::warn;

use oauth::{CALENDAR_SCOPE, MAIL_SCOPE, Stored, Tokens};

/// Runs until the account needs `heron auth google`, returning `Ok(())` after
/// setting `NeedsAuth`. Transient failures are retried inside; an `Err` makes
/// herond restart the provider after a delay.
pub async fn run(account: GoogleAccount, config: Arc<Config>, hub: Hub) -> Result<()> {
    let kinds: Vec<SourceKind> = [(account.mail, SourceKind::Gmail), (account.calendar, SourceKind::GoogleCalendar)]
        .into_iter()
        .filter_map(|(enabled, kind)| enabled.then_some(kind))
        .collect();
    if kinds.is_empty() {
        return Ok(());
    }
    let set_all = |state, message: &str| {
        for kind in &kinds {
            hub.set_status(&account.id, *kind, state, Some(message.to_owned()));
        }
    };

    let Some(client) = config.google_client.clone() else {
        set_all(SourceState::Error, "add [google_client] to config.toml");
        return Ok(());
    };
    let login_hint = format!("run `heron auth google {}`", account.id);
    let stored = match secrets::load::<Stored>(&oauth::secret_name(&account.id)) {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            set_all(SourceState::NeedsAuth, &login_hint);
            return Ok(());
        }
        Err(err) => {
            warn!("google/{}: {err:#}", account.id);
            set_all(SourceState::NeedsAuth, &format!("stored login unreadable; {login_hint}"));
            return Ok(());
        }
    };
    for kind in &kinds {
        hub.set_status(&account.id, *kind, SourceState::Connecting, None);
    }

    let http = oauth::http_client()?;
    let tokens = Arc::new(Tokens::new(http.clone(), client, stored.refresh_token.clone(), account.id.clone()));
    let missing = |scope: &str, kind| {
        hub.set_status(&account.id, kind, SourceState::NeedsAuth, Some(format!("access not granted; {login_hint}")));
        warn!("google/{}: the stored login lacks {scope}", account.id);
    };

    let mail_enabled = account.mail && stored.has(MAIL_SCOPE);
    if account.mail && !mail_enabled {
        missing(MAIL_SCOPE, SourceKind::Gmail);
    }
    let calendar_enabled = account.calendar && stored.has(CALENDAR_SCOPE);
    if account.calendar && !calendar_enabled {
        missing(CALENDAR_SCOPE, SourceKind::GoogleCalendar);
    }
    if !mail_enabled && !calendar_enabled {
        return Ok(());
    }

    let mail = async {
        if !mail_enabled {
            return std::future::pending().await;
        }
        gmail::run(account.clone(), config.mail.max_messages, tokens.clone(), hub.clone()).await
    };
    let calendar = async {
        if !calendar_enabled {
            return std::future::pending().await;
        }
        calendar::run(account.clone(), config.calendar.clone(), http.clone(), tokens.clone(), hub.clone()).await
    };
    let result = tokio::select! {
        result = mail => result,
        result = calendar => result,
    };
    let Err(err) = result else { return Ok(()) };
    match oauth::needs_auth(&err) {
        Some(reason) => {
            // A refused refresh token breaks both kinds, so mark both.
            set_all(SourceState::NeedsAuth, &reason.0);
            Ok(())
        }
        None => Err(err),
    }
}

/// Interactive OAuth login in the browser. Stores the refresh token for `run`.
pub async fn login(account: &GoogleAccount, client: &GoogleClient) -> Result<()> {
    login::login(account, client).await
}

/// Revokes and deletes the stored token.
pub async fn logout(account: &GoogleAccount) -> Result<()> {
    login::logout(account).await
}

/// Exponential delay between reconnects, from 5 seconds to 5 minutes.
pub(crate) struct Backoff {
    next: Duration,
}

impl Backoff {
    const MIN: Duration = Duration::from_secs(5);
    const MAX: Duration = Duration::from_secs(300);

    pub(crate) fn next(&mut self) -> Duration {
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }

    pub(crate) fn reset(&mut self) {
        self.next = Self::MIN;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self { next: Self::MIN }
    }
}

/// One line for the source status.
pub(crate) fn short(err: &anyhow::Error) -> String {
    let text = format!("{err:#}");
    match text.char_indices().nth(160) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let mut backoff = Backoff::default();
        let delays: Vec<u64> = (0..8).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 40, 80, 160, 300, 300]);
        backoff.reset();
        assert_eq!(backoff.next().as_secs(), 5);
    }
}
