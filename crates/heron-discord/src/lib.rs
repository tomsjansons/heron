//! Unread state of one Discord server, read-only, over the user gateway.
//!
//! Uses the owner's own user token. Nothing is ever sent that changes state
//! on Discord: no messages, no acks, no presence changes. One gateway
//! connection per daemon run, no REST polling: message history is fetched
//! once per unread channel (as opening it in the client would) and then kept
//! current from gateway events.

mod client;
mod gateway;
mod history;
mod messages;
mod permissions;
mod state;

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result};
use heron_core::config::DiscordConfig;
use heron_core::{Hub, SourceKind, SourceState, secrets};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tracing::{info, warn};

use gateway::{Event, Stop};
use state::GuildState;

const SECRET: &str = "discord";
const SOURCE: &str = "discord";
/// Coalesces bursts of events into one publish.
const DEBOUNCE: Duration = Duration::from_millis(250);
/// Recompute this often so timed mutes expire without an event.
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize)]
struct Credentials {
    token: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { token: <redacted> }")
    }
}

/// Aborts the gateway task when the provider is dropped, as on reload.
struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Runs until the token needs `heron auth discord`, returning `Ok(())` after
/// setting `NeedsAuth`. Transient failures are retried inside; an `Err` makes
/// herond restart the provider after a delay.
pub async fn run(config: DiscordConfig, hub: Hub) -> Result<()> {
    let status = |state, message: Option<String>| hub.set_status(SOURCE, SourceKind::Discord, state, message);

    let Some(credentials) = secrets::load::<Credentials>(SECRET)? else {
        status(SourceState::NeedsAuth, Some("run `heron auth discord`".into()));
        return Ok(());
    };
    let Ok(guild_id) = config.guild_id.trim().parse::<u64>() else {
        status(SourceState::Error, Some(format!("guild_id {:?} is not a Discord id", config.guild_id)));
        return Ok(());
    };

    status(SourceState::Connecting, None);
    let (tx, mut rx) = mpsc::channel(1024);
    let (fetch_tx, fetch_rx) = mpsc::unbounded_channel();
    let (history_tx, mut history_rx) = mpsc::channel(64);
    let _history = AbortOnDrop(tokio::spawn(history::run(credentials.token.clone(), guild_id, fetch_rx, history_tx)));
    let mut gateway = AbortOnDrop(tokio::spawn(gateway::run(credentials.token, guild_id, tx)));
    let mut guild = GuildState::new(guild_id);
    let mut publish_at: Option<Instant> = None;
    let mut refresh = tokio::time::interval(REFRESH);

    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Some(Event::Dispatch(kind, data)) => {
                    if guild.apply(&kind, &data) {
                        publish_at.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                    }
                    match kind.as_str() {
                        "READY" if guild.present() => status(SourceState::Ok, None),
                        "READY" => {
                            warn!("discord: guild {guild_id} is not in this account's server list");
                            status(SourceState::Error, Some(format!("not a member of guild {guild_id}")));
                        }
                        "GUILD_CREATE" | "RESUMED" if guild.present() => status(SourceState::Ok, None),
                        _ => {}
                    }
                }
                Some(Event::Disconnected(reason)) => status(SourceState::Error, Some(reason)),
                None => break,
            },
            Some(history::History { channel_id, result }) = history_rx.recv() => {
                if guild.load_history(channel_id, result, jiff::Timestamp::now()) {
                    publish_at.get_or_insert_with(|| Instant::now() + DEBOUNCE);
                }
            }
            _ = async { sleep_until(publish_at.unwrap_or_else(Instant::now)).await }, if publish_at.is_some() => {
                publish_at = None;
                let now = jiff::Timestamp::now();
                request_history(&mut guild, &fetch_tx, now);
                hub.set_discord(guild.snapshot(now));
            }
            _ = refresh.tick() => {
                let now = jiff::Timestamp::now();
                request_history(&mut guild, &fetch_tx, now);
                hub.set_discord(guild.snapshot(now));
            }
        }
    }

    // The gateway task dropped its sender, so it has finished.
    let result = (&mut gateway.0).await.context("discord gateway task panicked")?;
    hub.set_discord(None);
    match result? {
        Stop::AuthFailed => {
            info!("discord: token rejected");
            status(SourceState::NeedsAuth, Some("token rejected; run `heron auth discord`".into()));
            Ok(())
        }
        Stop::Closed => anyhow::bail!("discord gateway stopped"),
    }
}

/// Queues history fetches for unread channels whose messages are unknown.
fn request_history(guild: &mut GuildState, fetch_tx: &mpsc::UnboundedSender<u64>, now: jiff::Timestamp) {
    for channel in guild.take_fetches(now) {
        // The fetcher only stops with the provider.
        let _ = fetch_tx.send(channel);
    }
}

/// Prompts for a user token on the terminal, checks it, and stores it.
pub async fn login() -> Result<()> {
    println!("heron reads unread counts for one Discord server with your own user token.");
    println!("It never sends messages or marks anything read. Using a user token outside the");
    println!("official clients is against Discord's terms; you accept that risk.");
    println!();
    println!("To find the token: open https://discord.com/app in a browser and sign in,");
    println!("open the developer tools (F12) > Network, filter for \"api\", click any request");
    println!("and copy the value of its \"Authorization\" request header.");
    println!("Anyone with this token has full access to your account; keep it secret.");
    println!();

    let input = tokio::task::spawn_blocking(|| rpassword::prompt_password("Discord token: "))
        .await?
        .context("reading the token")?;
    let token = input.trim().trim_matches('"').to_owned();
    anyhow::ensure!(!token.is_empty(), "no token entered");

    let user = client::current_user(&token).await?.context("Discord rejected that token")?;
    let name = match &user.global_name {
        Some(global) => format!("{global} ({})", user.username),
        None => user.username.clone(),
    };
    secrets::store(SECRET, &Credentials { token })?;
    println!("Signed in as {name}");
    info!("discord: stored token for user {}", user.id);
    Ok(())
}

/// Deletes the stored token. Deliberately does not call Discord's logout,
/// which would end the browser session the token was copied from.
pub async fn logout() -> Result<()> {
    secrets::remove(SECRET)?;
    println!("Forgot the Discord token. It stays valid for your browser session.");
    Ok(())
}
