//! herond: runs one task per configured source, all reporting into a [`Hub`],
//! and serves the hub's snapshot over a unix socket.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use heron_core::{Config, Hub, Request, Response, Snapshot, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

/// Delay before restarting a provider that returned an error.
const RESTART_DELAY: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = Config::load()?;
    let hub = Hub::new(&config.calendar);
    let daemon = Arc::new(Daemon { hub: hub.clone(), providers: Mutex::new(JoinSet::new()) });
    daemon.start(Arc::new(config)).await;

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

    // Republish once a minute so events that have ended leave the agenda.
    let ticker = hub.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            ticker.tick();
        }
    });

    let mut terminate = signal(SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(err) = daemon.serve(stream).await {
                        warn!("client: {err:#}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = terminate.recv() => break,
        }
    }

    let _ = std::fs::remove_file(&path);
    Ok(())
}

struct Daemon {
    hub: Hub,
    providers: Mutex<JoinSet<()>>,
}

impl Daemon {
    /// Stops every provider and starts one task per configured source.
    async fn start(&self, config: Arc<Config>) {
        let mut providers = self.providers.lock().await;
        providers.abort_all();
        while providers.join_next().await.is_some() {}
        self.hub.reset(&config.calendar);
        self.hub.set_labels(config.labels());

        for account in &config.proton {
            let (account, config, hub) = (account.clone(), config.clone(), self.hub.clone());
            let name = format!("proton/{}", account.id);
            providers.spawn(supervise(name, move || heron_proton::run(account.clone(), config.clone(), hub.clone())));
        }
        for account in &config.google {
            let (account, config, hub) = (account.clone(), config.clone(), self.hub.clone());
            let name = format!("google/{}", account.id);
            providers.spawn(supervise(name, move || heron_google::run(account.clone(), config.clone(), hub.clone())));
        }
        if let Some(discord) = &config.discord {
            let (discord, hub) = (discord.clone(), self.hub.clone());
            providers.spawn(supervise("discord".into(), move || heron_discord::run(discord.clone(), hub.clone())));
        }
        info!(
            "started {} proton, {} google, {} discord",
            config.proton.len(),
            config.google.len(),
            usize::from(config.discord.is_some())
        );
    }

    async fn serve(&self, stream: UnixStream) -> Result<()> {
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

            let result = match request {
                Request::Status => {
                    let snapshot = self.hub.subscribe().borrow().clone();
                    return send(&mut write, &Response::Snapshot(snapshot)).await;
                }
                Request::Watch => return watch(&mut write, self.hub.subscribe()).await,
                Request::Ack { account } => self.hub.ack(account.as_deref()),
                Request::MarkRead { account, id } => self.hub.mark_read(&account, &id).await,
                Request::MarkUnread { account, id } => self.hub.mark_unread(&account, &id).await,
                Request::MarkSeen { account, id } => self.hub.mark_seen(&account, &id),
                Request::MarkUnseen { account, id } => self.hub.mark_unseen(&account, &id),
                Request::Message { account, id } => {
                    let response = match self.hub.fetch_message(&account, &id).await {
                        Ok(body) => Response::Message(body),
                        Err(err) => Response::Error { message: format!("{err:#}") },
                    };
                    send(&mut write, &response).await?;
                    continue;
                }
                Request::Event { id } => {
                    let response = match self.hub.event(&id) {
                        Some(event) => Response::Event(event),
                        None => Response::Error { message: format!("no upcoming event {id:?}") },
                    };
                    send(&mut write, &response).await?;
                    continue;
                }
                Request::Reload => match Config::load() {
                    Ok(config) => {
                        self.start(Arc::new(config)).await;
                        Ok(())
                    }
                    Err(err) => Err(err),
                },
            };
            let response = match result {
                Ok(()) => Response::Ok,
                Err(err) => Response::Error { message: format!("{err:#}") },
            };
            send(&mut write, &response).await?;
        }
        Ok(())
    }
}

/// Runs a provider, restarting it after an error. `Ok(())` means it is waiting
/// on the user (such as a login) and stays stopped until the next reload.
async fn supervise<F, Fut>(name: String, mut run: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    loop {
        match run().await {
            Ok(()) => {
                info!("{name}: stopped, waiting for reload");
                return;
            }
            Err(err) => {
                error!("{name}: {err:#}; restarting in {}s", RESTART_DELAY.as_secs());
                tokio::time::sleep(RESTART_DELAY).await;
            }
        }
    }
}

async fn watch(write: &mut OwnedWriteHalf, mut rx: tokio::sync::watch::Receiver<Snapshot>) -> Result<()> {
    loop {
        let snapshot = rx.borrow_and_update().clone();
        send(write, &Response::Snapshot(snapshot)).await?;
        if rx.changed().await.is_err() {
            return Ok(());
        }
    }
}

async fn send(write: &mut OwnedWriteHalf, response: &Response) -> Result<()> {
    let mut line = serde_json::to_vec(response)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok(())
}
