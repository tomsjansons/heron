//! herond: holds provider connections and serves the snapshot over a unix
//! socket. Providers are not wired up yet; the snapshot stays empty.

use std::sync::Arc;

use anyhow::{Context, Result};
use heron_core::{Request, Response, Snapshot, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let path = socket_path();
    if path.exists() {
        // A live daemon would answer; a stale socket file would not.
        if UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("herond is already running on {}", path.display());
        }
        std::fs::remove_file(&path).context("removing stale socket")?;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    info!("listening on {}", path.display());

    // Providers will publish through this sender; clients watch the receiver.
    let (tx, rx) = watch::channel(Snapshot::default());
    let tx = Arc::new(tx);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let rx = rx.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(err) = serve(stream, rx, tx).await {
                        warn!("client: {err:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }

    let _ = std::fs::remove_file(&path);
    Ok(())
}

async fn serve(
    stream: UnixStream,
    mut rx: watch::Receiver<Snapshot>,
    _tx: Arc<watch::Sender<Snapshot>>,
) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                send(&mut write, &Response::Error { message: err.to_string() }).await?;
                continue;
            }
        };

        match request {
            Request::Status => {
                let snapshot = rx.borrow_and_update().clone();
                send(&mut write, &Response::Snapshot(snapshot)).await?;
                return Ok(());
            }
            Request::Watch => loop {
                let snapshot = rx.borrow_and_update().clone();
                send(&mut write, &Response::Snapshot(snapshot)).await?;
                if rx.changed().await.is_err() {
                    return Ok(());
                }
            },
            Request::Ack { .. } | Request::MarkRead { .. } | Request::MarkUnread { .. } => {
                send(&mut write, &Response::Error { message: "no providers configured yet".into() }).await?;
            }
        }
    }
    Ok(())
}

async fn send(write: &mut tokio::net::unix::OwnedWriteHalf, response: &Response) -> Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok(())
}
