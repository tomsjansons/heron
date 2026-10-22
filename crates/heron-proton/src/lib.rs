//! Proton Mail and Proton Calendar through the Proton API, without Bridge.
//!
//! `heron auth proton <id>` logs in with SRP and stores the session plus the
//! derived key passphrase (never the password). `run` then follows the inbox
//! through the core event loop and polls calendars, decrypting events with
//! the account keys.

mod api;
mod auth;
mod calendar;
mod ical;
mod keys;
mod mail;

use std::sync::Arc;
use std::time::Duration;

use heron_core::config::{Config, ProtonAccount};
use heron_core::{Hub, SourceKind, SourceState, secrets};
use tracing::info;

use api::{Api, Session, needs_auth};

/// Runs until the account needs `heron auth proton`, returning `Ok(())` after
/// setting `NeedsAuth`. Transient failures are retried inside; an `Err` makes
/// herond restart the provider after a delay.
pub async fn run(account: ProtonAccount, config: Arc<Config>, hub: Hub) -> anyhow::Result<()> {
    let kinds: Vec<SourceKind> = [(account.mail, SourceKind::ProtonMail), (account.calendar, SourceKind::ProtonCalendar)]
        .into_iter()
        .filter_map(|(enabled, kind)| enabled.then_some(kind))
        .collect();
    if kinds.is_empty() {
        return Ok(());
    }
    let login_hint = format!("run `heron auth proton {}`", account.id);
    let set_all = |state: SourceState, message: Option<String>| {
        for kind in &kinds {
            hub.set_status(&account.id, *kind, state, message.clone());
        }
    };

    let name = auth::secret_name(&account);
    let Some(session) = secrets::load::<Session>(&name)? else {
        set_all(SourceState::NeedsAuth, Some(login_hint));
        return Ok(());
    };
    set_all(SourceState::Connecting, None);
    let api = Api::new(Some(session), Some(name))?;

    let mut commands = account.mail.then(|| hub.register_mail(&account.id));
    let mail = async {
        match commands.as_mut() {
            Some(commands) => mail::run(&api, &account.id, config.mail.max_messages, &hub, commands).await,
            None => std::future::pending().await,
        }
    };
    let calendar = async {
        if account.calendar {
            calendar::run(&api, &account.id, config.calendar.days_ahead, config.calendar.poll_secs, &hub).await
        } else {
            std::future::pending().await
        }
    };

    // Both only return once the session is unusable.
    let err = tokio::select! {
        err = mail => err,
        err = calendar => err,
    };
    let reason = needs_auth(&err).map_or_else(|| format!("{err:#}"), ToString::to_string);
    info!("proton/{}: {reason}; waiting for login", account.id);
    set_all(SourceState::NeedsAuth, Some(format!("{reason}; {login_hint}")));
    Ok(())
}

/// Interactive login on the terminal. Stores the session for `run`.
pub async fn login(account: &ProtonAccount) -> anyhow::Result<()> {
    auth::login(account).await
}

/// Revokes and deletes the stored session.
pub async fn logout(account: &ProtonAccount) -> anyhow::Result<()> {
    auth::logout(account).await
}

/// Retry delay doubling from 15s to 5 minutes.
#[derive(Default)]
struct Backoff {
    failures: u32,
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let secs = 15u64.saturating_mul(1 << self.failures.min(5)).min(300);
        self.failures += 1;
        Duration::from_secs(secs)
    }

    fn reset(&mut self) {
        self.failures = 0;
    }
}

/// One-line status message: the innermost cause, which is the most specific.
fn short(err: &anyhow::Error) -> String {
    let text = err.root_cause().to_string();
    match text.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_resets() {
        let mut backoff = Backoff::default();
        let delays: Vec<u64> = (0..7).map(|_| backoff.next().as_secs()).collect();
        assert_eq!(delays, [15, 30, 60, 120, 240, 300, 300]);
        backoff.reset();
        assert_eq!(backoff.next().as_secs(), 15);
    }
}
