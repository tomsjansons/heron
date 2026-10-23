//! Marking channels read, the one change heron makes on Discord, sent only
//! when the user asks. One channel goes through the same endpoint the web
//! client uses when a channel is opened; several go in one bulk request, as
//! "Mark as read" on a server does.

use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::time::sleep;
use tracing::{debug, warn};

use crate::client;
use crate::history::{header_seconds, retry_after};

/// Read states per bulk request, Discord's limit.
const BULK: usize = 100;
const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Marks each `(channel, message)` read up to and including the message.
pub async fn ack(client: &reqwest::Client, token: &str, guild_id: u64, targets: &[(u64, u64)]) -> Result<()> {
    match targets {
        [] => Ok(()),
        [(channel, message)] => {
            let url = format!("{}/channels/{channel}/messages/{message}/ack", client::API);
            let referer = format!("{}/channels/{guild_id}/{channel}", client::ORIGIN);
            send(client, token, &url, &referer, &json!({"token": null})).await
        }
        _ => {
            let url = format!("{}/read-states/ack-bulk", client::API);
            let referer = format!("{}/channels/{guild_id}", client::ORIGIN);
            for chunk in targets.chunks(BULK) {
                let read_states: Vec<Value> = chunk.iter()
                    .map(|(channel, message)| json!({
                        "channel_id": channel.to_string(),
                        "message_id": message.to_string(),
                        "read_state_type": 0,
                    }))
                    .collect();
                send(client, token, &url, &referer, &json!({"read_states": read_states})).await?;
            }
            Ok(())
        }
    }
}

async fn send(client: &reqwest::Client, token: &str, url: &str, referer: &str, body: &Value) -> Result<()> {
    for _ in 0..MAX_ATTEMPTS {
        let response = client::post(client, token, url, referer, body).send().await
            .map_err(|err| anyhow::anyhow!("contacting discord.com: {}", err.without_url()))?;
        let status = response.status().as_u16();
        match status {
            200..=299 => {
                let count = body.get("read_states").and_then(Value::as_array).map_or(1, Vec::len);
                debug!("discord: acked {count} read state(s)");
                return Ok(());
            }
            429 => {
                let header = header_seconds(response.headers(), "retry-after");
                let body = response.json::<Value>().await.ok();
                let retry = retry_after(body.as_ref(), header).min(MAX_RETRY_AFTER);
                warn!("discord ack: rate limited, waiting {:.1}s", retry.as_secs_f64());
                sleep(retry).await;
            }
            401 => bail!("Discord rejected the token; run `heron auth discord`"),
            _ => bail!("Discord answered HTTP {status}"),
        }
    }
    bail!("Discord kept rate limiting the request")
}
