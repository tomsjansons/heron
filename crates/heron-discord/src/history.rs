//! Message history for unread channels, fetched once per channel as the web
//! client does when a channel is opened: `GET /channels/{id}/messages`.
//! Requests run one at a time with a pause between them and honour 429s
//! and the rate-limit headers. Nothing here polls; [`crate::state`] decides
//! which channels need history.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, sleep_until};
use tracing::{debug, warn};

use crate::client;

/// The newest page of a channel; enough to count up to the unread cap.
pub const PAGE: usize = heron_core::DISCORD_UNREAD_CAP as usize;
/// Pause between requests.
const DELAY: Duration = Duration::from_millis(400);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
const MAX_ATTEMPTS: u32 = 3;

#[derive(Debug)]
pub struct History {
    pub channel_id: u64,
    pub result: Fetched,
}

#[derive(Debug)]
pub enum Fetched {
    /// Messages, newest first, as Discord returns them.
    Messages(Vec<Value>),
    /// 403/404: the channel cannot be read; don't ask again.
    Unavailable,
    /// Network error, 5xx, repeated 429; may be retried later.
    Failed,
}

/// Fetches the channels sent on `rx`, in order and without duplicates,
/// reporting each on `tx`. Ends when either side closes.
pub async fn run(token: String, guild_id: u64, mut rx: mpsc::UnboundedReceiver<u64>, tx: mpsc::Sender<History>) {
    let client = match client::http() {
        Ok(client) => client,
        Err(err) => {
            warn!("discord history: no http client: {err:#}");
            return;
        }
    };
    let mut queue = VecDeque::new();
    let mut queued = HashSet::new();
    let mut next = Instant::now();
    loop {
        if queue.is_empty() {
            let Some(id) = rx.recv().await else { return };
            enqueue(&mut queue, &mut queued, id);
        }
        while let Ok(id) = rx.try_recv() {
            enqueue(&mut queue, &mut queued, id);
        }
        let Some(channel_id) = queue.pop_front() else { continue };
        queued.remove(&channel_id);
        sleep_until(next).await;
        let (result, wait) = fetch(&client, &token, guild_id, channel_id).await;
        let jitter = Duration::from_millis((rand::random::<f64>() * 100.0) as u64);
        next = Instant::now() + wait.max(DELAY) + jitter;
        if tx.send(History { channel_id, result }).await.is_err() {
            return;
        }
    }
}

fn enqueue(queue: &mut VecDeque<u64>, queued: &mut HashSet<u64>, id: u64) {
    if queued.insert(id) {
        queue.push_back(id);
    }
}

/// One channel's newest page, plus how long to wait before the next request.
async fn fetch(client: &reqwest::Client, token: &str, guild_id: u64, channel_id: u64) -> (Fetched, Duration) {
    let url = format!("{}/channels/{channel_id}/messages?limit={PAGE}", client::API);
    let referer = format!("{}/channels/{guild_id}/{channel_id}", client::ORIGIN);
    for _ in 0..MAX_ATTEMPTS {
        debug!("discord history: fetching channel {channel_id}");
        let response = match client::get(client, token, &url, &referer).send().await {
            Ok(response) => response,
            Err(err) => {
                warn!("discord history: channel {channel_id}: {}", err.without_url());
                return (Fetched::Failed, DELAY);
            }
        };
        let status = response.status().as_u16();
        let wait = bucket_wait(response.headers());
        match status {
            200 => {
                return match response.json::<Vec<Value>>().await {
                    Ok(messages) => (Fetched::Messages(messages), wait),
                    Err(err) => {
                        warn!("discord history: channel {channel_id}: unreadable response: {}", err.without_url());
                        (Fetched::Failed, wait)
                    }
                };
            }
            429 => {
                let header = header_seconds(response.headers(), "retry-after");
                let body = response.json::<Value>().await.ok();
                let retry = retry_after(body.as_ref(), header).min(MAX_RETRY_AFTER);
                warn!("discord history: rate limited, waiting {:.1}s", retry.as_secs_f64());
                sleep(retry).await;
            }
            403 | 404 => {
                debug!("discord history: channel {channel_id} not readable (HTTP {status})");
                return (Fetched::Unavailable, wait);
            }
            _ => {
                warn!("discord history: channel {channel_id}: HTTP {status}");
                return (Fetched::Failed, wait);
            }
        }
    }
    (Fetched::Failed, DELAY)
}

/// The rest of the rate-limit bucket's window when it is exhausted.
pub(crate) fn bucket_wait(headers: &reqwest::header::HeaderMap) -> Duration {
    let remaining = headers.get("x-ratelimit-remaining").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    match remaining {
        Some(0) => header_seconds(headers, "x-ratelimit-reset-after").unwrap_or(DELAY).min(MAX_RETRY_AFTER),
        _ => Duration::ZERO,
    }
}

pub(crate) fn header_seconds(headers: &reqwest::header::HeaderMap, name: &str) -> Option<Duration> {
    let seconds = headers.get(name)?.to_str().ok()?.parse::<f64>().ok()?;
    Duration::try_from_secs_f64(seconds).ok()
}

/// A 429's `retry_after` (seconds, fractional) from the body, else the
/// header, else five seconds.
pub(crate) fn retry_after(body: Option<&Value>, header: Option<Duration>) -> Duration {
    body.and_then(|b| b.get("retry_after")).and_then(Value::as_f64)
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .or(header)
        .unwrap_or(Duration::from_secs(5))
        + Duration::from_millis(100)
}

#[cfg(test)]
mod tests {
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::json;

    use super::*;

    #[test]
    fn rate_limit_parsing() {
        let body = json!({"message": "You are being rate limited.", "retry_after": 1.5, "global": false});
        assert_eq!(retry_after(Some(&body), None), Duration::from_millis(1600));
        assert_eq!(retry_after(None, Some(Duration::from_secs(2))), Duration::from_millis(2100));
        assert_eq!(retry_after(None, None), Duration::from_millis(5100));

        let mut headers = HeaderMap::new();
        assert_eq!(bucket_wait(&headers), Duration::ZERO);
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
        headers.insert("x-ratelimit-reset-after", HeaderValue::from_static("2.25"));
        assert_eq!(bucket_wait(&headers), Duration::from_millis(2250));
        headers.insert("x-ratelimit-remaining", HeaderValue::from_static("3"));
        assert_eq!(bucket_wait(&headers), Duration::ZERO);
    }

    #[test]
    fn queue_dedupes() {
        let (mut queue, mut queued) = (VecDeque::new(), HashSet::new());
        for id in [1, 2, 1, 3, 2] {
            enqueue(&mut queue, &mut queued, id);
        }
        assert_eq!(queue, [1, 2, 3]);
    }
}
