//! Proton Mail and Proton Calendar through the Proton API, without Bridge.

use heron_core::config::{Config, ProtonAccount};
use heron_core::{Hub, SourceKind, SourceState};

/// Runs until the account needs `heron auth proton`, returning `Ok(())` after
/// setting `NeedsAuth`. Transient failures are retried inside; an `Err` makes
/// herond restart the provider after a delay.
pub async fn run(account: ProtonAccount, _config: std::sync::Arc<Config>, hub: Hub) -> anyhow::Result<()> {
    hub.set_status(&account.id, SourceKind::ProtonMail, SourceState::Error, Some("not implemented".into()));
    Ok(())
}

/// Interactive login on the terminal. Stores the session for `run`.
pub async fn login(_account: &ProtonAccount) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}

/// Revokes and deletes the stored session.
pub async fn logout(_account: &ProtonAccount) -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}
