//! One user-account gateway session: identify, heartbeat, resume, reconnect.
//! It only listens; the only payloads sent are Identify/Resume, heartbeats
//! and one guild subscription so large guilds deliver message events.
//!
//! The connection lifecycle, Identify payload, close-code handling and
//! `zlib-stream` decoding are adapted from concord
//! (github.com/chojs23/concord, GPL-3.0), `src/discord/gateway.rs`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use flate2::{Decompress, FlushDecompress};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, sleep_until, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{debug, info, warn};

use crate::client;

const QUERY: &str = "/?v=9&encoding=json&compress=zlib-stream";
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=9&encoding=json&compress=zlib-stream";

/// LAZY_USER_NOTES, VERSIONED_READ_STATES, VERSIONED_USER_GUILD_SETTINGS,
/// DEDUPE_USER_OBJECTS, PRIORITIZED_READY_PAYLOAD,
/// MULTIPLE_GUILD_EXPERIMENT_POPULATIONS, NON_CHANNEL_READ_STATES, as concord.
const CAPABILITIES: u64 = 253;

/// READY for large accounts exceeds tungstenite's 16 MiB frame default.
const SIZE_LIMIT: usize = 64 << 20;
const ZLIB_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BACKOFF_SECS: u64 = 60;
/// Failed connections to the resume URL before starting a new session.
const MAX_RESUME_ATTEMPTS: u32 = 3;

#[derive(Debug)]
pub enum Event {
    Dispatch(String, Value),
    /// The connection dropped; a reconnect follows after a delay.
    Disconnected(String),
}

/// Why [`run`] returned without error.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The receiver went away.
    Closed,
    /// Close code 4004: the token is no longer valid.
    AuthFailed,
}

#[derive(Default)]
struct Session {
    id: Option<String>,
    resume_url: Option<String>,
    seq: Option<u64>,
    /// The current connection reached READY or RESUMED.
    established: bool,
}

impl Session {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn resumable(&self) -> bool {
        self.id.is_some() && self.resume_url.is_some() && self.seq.is_some()
    }
}

/// What to do after a connection ends.
#[derive(Debug, PartialEq, Eq)]
enum Exit {
    Resume(String),
    Reidentify(String),
    AuthFailed,
    Fatal(String),
    Closed,
}

/// Keeps one session alive, sending dispatches to `tx`, until the receiver
/// is dropped or the token is rejected. `Err` for unrecoverable rejections.
pub async fn run(token: String, guild_id: u64, tx: mpsc::Sender<Event>) -> Result<Stop> {
    let mut session = Session::default();
    let mut failures = 0u32;
    let mut resume_failures = 0u32;
    loop {
        let resuming = session.resumable();
        let reason = match connect(&token, guild_id, &mut session, &tx).await {
            Ok(Exit::Closed) => return Ok(Stop::Closed),
            Ok(Exit::AuthFailed) => return Ok(Stop::AuthFailed),
            Ok(Exit::Fatal(reason)) => bail!(reason),
            Ok(Exit::Resume(reason)) => reason,
            Ok(Exit::Reidentify(reason)) => {
                session.clear();
                reason
            }
            Err(err) => format!("{err:#}"),
        };
        if std::mem::take(&mut session.established) {
            failures = 0;
            resume_failures = 0;
        } else if resuming {
            resume_failures += 1;
            if resume_failures >= MAX_RESUME_ATTEMPTS {
                session.clear();
                resume_failures = 0;
            }
        }
        failures += 1;
        let delay = backoff(failures);
        warn!("gateway: {reason}; reconnecting in {}s", delay.as_secs());
        if tx.send(Event::Disconnected(reason)).await.is_err() {
            return Ok(Stop::Closed);
        }
        sleep(delay).await;
    }
}

/// 1, 2, 4 … 60 seconds, plus up to a second of jitter.
fn backoff(failures: u32) -> Duration {
    let base = 1u64.checked_shl(failures.saturating_sub(1)).unwrap_or(u64::MAX).min(MAX_BACKOFF_SECS);
    Duration::from_secs(base) + Duration::from_millis((rand::random::<f64>() * 1000.0) as u64)
}

async fn connect(token: &str, guild_id: u64, session: &mut Session, tx: &mpsc::Sender<Event>) -> Result<Exit> {
    let resume = session.resumable();
    let url = match &session.resume_url {
        Some(base) if resume && base.starts_with("wss://") => format!("{}{QUERY}", base.trim_end_matches('/')),
        _ => GATEWAY_URL.to_owned(),
    };
    debug!("gateway: connecting to {url} (resume: {resume})");

    let mut request = url.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert("User-Agent", HeaderValue::from_static(client::USER_AGENT));
    headers.insert("Origin", HeaderValue::from_static(client::ORIGIN));
    headers.insert("Accept-Language", HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Pragma", HeaderValue::from_static("no-cache"));
    let config = WebSocketConfig::default().max_message_size(Some(SIZE_LIMIT)).max_frame_size(Some(SIZE_LIMIT));
    let (ws, _) = timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async_with_config(request, Some(config), false))
        .await
        .context("gateway connect timed out")?
        .context("connecting to the gateway")?;
    let (mut sink, mut stream) = ws.split();
    let mut inflater = ZlibStream::default();

    let hello = match timeout(CONNECT_TIMEOUT, next_frame(&mut stream, &mut inflater)).await.context("no HELLO")?? {
        Frame::Payload(hello) => hello,
        Frame::Closed(code, reason) => return Ok(close_exit(code, &reason)),
    };
    let interval = hello.pointer("/d/heartbeat_interval").and_then(Value::as_u64)
        .filter(|_| hello.get("op").and_then(Value::as_u64) == Some(10))
        .context("first gateway payload was not HELLO")?;
    let interval = Duration::from_millis(interval);

    let handshake = match (&session.id, session.seq) {
        (Some(id), Some(seq)) if resume => json!({"op": 6, "d": {"token": token, "session_id": id, "seq": seq}}),
        _ => identify(token),
    };
    sink.send(Message::Text(handshake.to_string().into())).await.context("sending identify")?;

    // The first heartbeat is jittered so reconnecting clients spread out.
    let mut next_beat = Instant::now() + interval.mul_f64(rand::random::<f64>());
    let mut acked = true;
    loop {
        tokio::select! {
            _ = sleep_until(next_beat) => {
                if !acked {
                    return Ok(Exit::Resume("heartbeat not acknowledged".into()));
                }
                sink.send(heartbeat(session.seq)).await.context("sending heartbeat")?;
                acked = false;
                next_beat = Instant::now() + interval;
            }
            frame = next_frame(&mut stream, &mut inflater) => {
                let payload = match frame? {
                    Frame::Payload(payload) => payload,
                    Frame::Closed(code, reason) => return Ok(close_exit(code, &reason)),
                };
                match payload.get("op").and_then(Value::as_u64) {
                    Some(0) => {
                        if let Some(seq) = payload.get("s").and_then(Value::as_u64) {
                            session.seq = Some(seq);
                        }
                        let kind = payload.get("t").and_then(Value::as_str).unwrap_or_default().to_owned();
                        let data = payload.get("d").cloned().unwrap_or(Value::Null);
                        match kind.as_str() {
                            "READY" => {
                                session.id = data.get("session_id").and_then(Value::as_str).map(str::to_owned);
                                session.resume_url = data.get("resume_gateway_url").and_then(Value::as_str).map(str::to_owned);
                                session.established = true;
                                info!("gateway: ready");
                                if in_guild(&data, guild_id) {
                                    sink.send(Message::Text(subscribe(guild_id).to_string().into())).await
                                        .context("sending guild subscription")?;
                                }
                            }
                            "RESUMED" => {
                                session.established = true;
                                info!("gateway: resumed");
                            }
                            _ => {}
                        }
                        if tx.send(Event::Dispatch(kind, data)).await.is_err() {
                            return Ok(Exit::Closed);
                        }
                    }
                    Some(1) => sink.send(heartbeat(session.seq)).await.context("sending heartbeat")?,
                    Some(7) => return Ok(Exit::Resume("server requested reconnect".into())),
                    Some(9) if payload.get("d").and_then(Value::as_bool) == Some(true) => {
                        return Ok(Exit::Resume("session invalidated (resumable)".into()));
                    }
                    Some(9) => return Ok(Exit::Reidentify("session invalidated".into())),
                    Some(11) => acked = true,
                    other => debug!("gateway: ignoring op {other:?}"),
                }
            }
        }
    }
}

fn heartbeat(seq: Option<u64>) -> Message {
    Message::Text(json!({"op": 1, "d": seq}).to_string().into())
}

/// Identify as the web client. Presence `unknown` keeps the user's saved
/// status instead of setting one.
fn identify(token: &str) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": token,
            "capabilities": CAPABILITIES,
            "properties": client::properties(),
            "presence": {"status": "unknown", "since": 0, "activities": [], "afk": false},
            "compress": false,
            "client_state": {
                "guild_versions": {},
                "highest_last_message_id": "0",
                "read_state_version": 0,
                "user_guild_settings_version": -1,
                "user_settings_version": -1,
                "private_channels_version": "0",
                "api_code_version": 0,
            },
        },
    })
}

/// Op 37, as the client sends when a guild is opened. Guilds above the
/// large threshold (250 members for users) only deliver message events to
/// subscribed sessions.
fn subscribe(guild_id: u64) -> Value {
    json!({
        "op": 37,
        "d": {"subscriptions": {guild_id.to_string(): {"typing": true, "threads": true, "activities": true}}},
    })
}

fn in_guild(ready: &Value, guild_id: u64) -> bool {
    let id = guild_id.to_string();
    ready.get("guilds").and_then(Value::as_array).into_iter().flatten()
        .any(|g| g.get("id").and_then(Value::as_str) == Some(id.as_str()))
}

fn close_exit(code: Option<u16>, reason: &str) -> Exit {
    let message = match code {
        Some(code) => format!("gateway closed ({code} {reason})"),
        None => "gateway closed".to_owned(),
    };
    match code {
        Some(4004) => Exit::AuthFailed,
        Some(4010..=4014) => Exit::Fatal(message),
        Some(4003 | 4007 | 4009 | 4015 | 4016) | Some(1000 | 1001) => Exit::Reidentify(message),
        _ => Exit::Resume(message),
    }
}

enum Frame {
    Payload(Value),
    Closed(Option<u16>, String),
}

/// Reads until one whole payload is available. Cancel-safe: partial
/// compressed input is kept in `inflater`.
async fn next_frame<S>(stream: &mut S, inflater: &mut ZlibStream) -> Result<Frame>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await.context("gateway connection ended")?.context("reading from the gateway")? {
            Message::Text(text) => return Ok(Frame::Payload(serde_json::from_str(&text)?)),
            Message::Binary(chunk) => {
                if let Some(text) = inflater.push(&chunk)? {
                    return Ok(Frame::Payload(serde_json::from_slice(&text).context("parsing gateway payload")?));
                }
            }
            Message::Close(frame) => {
                return Ok(match frame {
                    Some(f) => Frame::Closed(Some(u16::from(f.code)), f.reason.to_string()),
                    None => Frame::Closed(None, String::new()),
                });
            }
            _ => {}
        }
    }
}

/// Discord's `zlib-stream` transport: one inflater for the whole connection,
/// each payload ends with a sync flush and may span several messages.
pub struct ZlibStream {
    inflater: Decompress,
    pending: Vec<u8>,
}

impl Default for ZlibStream {
    fn default() -> Self {
        Self { inflater: Decompress::new(true), pending: Vec::new() }
    }
}

impl ZlibStream {
    /// Returns a decompressed payload once `chunk` completes one.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.pending.len() + chunk.len() > SIZE_LIMIT {
            bail!("compressed gateway payload exceeds {SIZE_LIMIT} bytes");
        }
        self.pending.extend_from_slice(chunk);
        if !self.pending.ends_with(&ZLIB_SUFFIX) {
            return Ok(None);
        }
        let input = std::mem::take(&mut self.pending);
        let mut output = Vec::with_capacity(input.len() * 4 + 1024);
        let mut offset = 0;
        loop {
            if output.capacity() - output.len() < 8192 {
                output.reserve(output.capacity());
            }
            let before = self.inflater.total_in();
            self.inflater.decompress_vec(&input[offset..], &mut output, FlushDecompress::Sync)
                .context("inflating gateway payload")?;
            offset += (self.inflater.total_in() - before) as usize;
            // Done once all input is consumed and the inflater stopped short
            // of filling the buffer.
            if offset >= input.len() && output.len() < output.capacity() {
                return Ok(Some(output));
            }
            if output.len() > SIZE_LIMIT {
                bail!("gateway payload exceeds {SIZE_LIMIT} bytes");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use flate2::{Compress, Compression, FlushCompress};

    use super::*;

    fn compress_message(compressor: &mut Compress, data: &[u8]) -> Vec<u8> {
        let start = compressor.total_in();
        let mut out = Vec::with_capacity(data.len() + 1024);
        loop {
            let consumed = (compressor.total_in() - start) as usize;
            compressor.compress_vec(&data[consumed..], &mut out, FlushCompress::Sync).unwrap();
            if (compressor.total_in() - start) as usize == data.len() && out.len() < out.capacity() {
                return out;
            }
            out.reserve(out.capacity().max(1024));
        }
    }

    #[test]
    fn zlib_stream_across_payloads_and_chunks() {
        let mut compressor = Compress::new(Compression::default(), true);
        let first = br#"{"op":10,"d":{"heartbeat_interval":41250}}"#.to_vec();
        let second: Vec<u8> = format!(r#"{{"op":0,"t":"READY","d":{{"pad":"{}"}}}}"#, "x".repeat(200_000)).into_bytes();

        let mut stream = ZlibStream::default();
        let a = compress_message(&mut compressor, &first);
        assert_eq!(stream.push(&a).unwrap(), Some(first));

        // The second payload arrives in three websocket messages.
        let b = compress_message(&mut compressor, &second);
        let (x, rest) = b.split_at(b.len() / 3);
        let (y, z) = rest.split_at(rest.len() / 2);
        assert_eq!(stream.push(x).unwrap(), None);
        assert_eq!(stream.push(y).unwrap(), None);
        assert_eq!(stream.push(z).unwrap(), Some(second));
    }

    #[test]
    fn close_codes() {
        assert_eq!(close_exit(Some(4004), "Authentication failed."), Exit::AuthFailed);
        assert!(matches!(close_exit(Some(4013), ""), Exit::Fatal(_)));
        assert!(matches!(close_exit(Some(4009), ""), Exit::Reidentify(_)));
        assert!(matches!(close_exit(Some(4000), ""), Exit::Resume(_)));
        assert!(matches!(close_exit(None, ""), Exit::Resume(_)));
    }

    #[test]
    fn identify_payload() {
        let payload = identify("secret");
        assert_eq!(payload["op"], 2);
        assert_eq!(payload["d"]["capabilities"], CAPABILITIES);
        assert_eq!(payload["d"]["properties"]["browser"], "Chrome");
        assert_eq!(payload["d"]["presence"]["status"], "unknown");
        assert!(payload["d"].get("intents").is_none());
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(1) < Duration::from_secs(2));
        assert!(backoff(3) >= Duration::from_secs(4));
        assert!(backoff(100) <= Duration::from_secs(MAX_BACKOFF_SECS + 1));
    }

    #[test]
    fn subscription_only_for_member_guilds() {
        let ready = json!({"guilds": [{"id": "1000"}]});
        assert!(in_guild(&ready, 1000));
        assert!(!in_guild(&ready, 1001));
        assert_eq!(subscribe(1000)["d"]["subscriptions"]["1000"]["typing"], true);
    }
}
