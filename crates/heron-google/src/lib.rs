//! Gmail over IMAP with XOAUTH2, and Google Calendar through Calendar API v3.

use heron_core::config::{Config, GoogleAccount, GoogleClient};
use heron_core::{Hub, SourceKind, SourceState};

/// Runs until the account needs `heron auth google`, returning `Ok(())` after
/// setting `NeedsAuth`. Transient failures are retried inside; an `Err` makes
/// herond restart the provider after a delay.
pub async fn run(account: GoogleAccount, _config: std::sync::Arc<Config>, hub: Hub) -> anyhow::Result<()> {
    hub.set_status(&account.id, SourceKind::Gmail, SourceState::Error, Some("not implemented".into()));
    Ok(())
}

/// Interactive OAuth login in the browser. Stores the refresh token for `run`.
pub async fn login(_account: &GoogleAccount, _client: &GoogleClient) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}

/// Revokes and deletes the stored token.
pub async fn logout(_account: &GoogleAccount) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}
