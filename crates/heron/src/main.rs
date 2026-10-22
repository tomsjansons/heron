//! heron: CLI client for herond. Every command is one request over the socket;
//! responses are printed as JSON lines so the Noctalia plugin can consume them.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use heron_core::{Request, Response, socket_path};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Parser)]
#[command(version, about = "Mail, calendar and Discord status from herond")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the current snapshot as one JSON line.
    Status,
    /// Print the snapshot, then a new line on every change.
    Watch,
    /// Clear "new" for one account, or all of them.
    Ack { account: Option<String> },
    MarkRead { account: String, id: String },
    MarkUnread { account: String, id: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    let request = match Cli::parse().command {
        Command::Status => Request::Status,
        Command::Watch => Request::Watch,
        Command::Ack { account } => Request::Ack { account },
        Command::MarkRead { account, id } => Request::MarkRead { account, id },
        Command::MarkUnread { account, id } => Request::MarkUnread { account, id },
    };
    let streaming = matches!(request, Request::Watch);

    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("connecting to herond at {}", path.display()))?;
    let (read, mut write) = stream.into_split();

    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    write.write_all(&line).await?;

    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        println!("{line}");
        if let Ok(Response::Error { message }) = serde_json::from_str(&line) {
            anyhow::bail!(message);
        }
        if !streaming {
            break;
        }
    }
    Ok(())
}
