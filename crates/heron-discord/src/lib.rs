//! Unread state of one Discord server, read-only, over the user gateway.

use heron_core::config::DiscordConfig;
use heron_core::{Hub, SourceKind, SourceState};

/// Runs until the token needs `heron auth discord`, returning `Ok(())` after
/// setting `NeedsAuth`. Transient failures are retried inside; an `Err` makes
/// herond restart the provider after a delay.
pub async fn run(_config: DiscordConfig, hub: Hub) -> anyhow::Result<()> {
    hub.set_status("discord", SourceKind::Discord, SourceState::Error, Some("not implemented".into()));
    Ok(())
}

/// Prompts for a user token on the terminal, checks it, and stores it.
pub async fn login() -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}

/// Deletes the stored token.
pub async fn logout() -> anyhow::Result<()> {
    anyhow::bail!("not implemented")
}
