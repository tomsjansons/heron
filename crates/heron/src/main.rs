//! heron: CLI for herond. Talks to the daemon over its socket, and runs the
//! interactive logins that store credentials for it.

mod init;

use std::io::IsTerminal;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use heron_core::{Config, Request, Response, Snapshot, SourceState, paths, socket_path};
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
    /// Create the config file by answering a few questions.
    Init {
        /// Overwrite an existing config.
        #[arg(long)]
        force: bool,
    },
    /// Show counts, upcoming events and source health.
    Status {
        /// Print the raw snapshot as one JSON line.
        #[arg(long)]
        json: bool,
    },
    /// Print the snapshot as JSON, then a new line on every change.
    Watch {
        /// Keep only the newest N messages per account, for consumers with
        /// tight budgets such as the shell plugin.
        #[arg(long)]
        messages: Option<usize>,
        /// Keep only the first N unread Discord channels (most mentions first).
        #[arg(long)]
        channels: Option<usize>,
        /// Keep each line under N bytes by dropping Discord embeds, then
        /// channels, from the end of the list.
        #[arg(long)]
        max_bytes: Option<usize>,
    },
    /// Clear "new" for one account, or every account.
    Ack { account: Option<String> },
    /// Mark a message read on the server.
    MarkRead { account: String, id: String },
    /// Mark a message unread on the server.
    MarkUnread { account: String, id: String },
    /// Move a message to the account's trash.
    #[command(alias = "delete")]
    Trash { account: String, id: String },
    /// Clear "new" for one message, leaving it unread.
    MarkSeen { account: String, id: String },
    /// Make one message "new" again until it is marked seen.
    MarkUnseen { account: String, id: String },
    /// Mark a Discord channel read on Discord, or every unread channel.
    DiscordRead {
        /// Channel id, as in `heron status --json`.
        #[arg(required_unless_present = "all")]
        channel: Option<String>,
        /// Mark every listed channel read.
        #[arg(long, conflicts_with = "channel")]
        all: bool,
    },
    /// Show a message's content without marking it read.
    Message {
        account: String,
        id: String,
        /// Print the message as one JSON line.
        #[arg(long)]
        json: bool,
        /// Cut the text after this many characters (0: no limit).
        #[arg(long, default_value_t = 0)]
        max_chars: usize,
    },
    /// Show one upcoming calendar event with its details.
    Event {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Re-read the config and restart every source.
    Reload,
    /// Log in to a source and store its credentials.
    Auth {
        provider: AuthProvider,
        /// Account id from the config. Not used for discord.
        account: Option<String>,
    },
    /// Revoke and delete a source's stored credentials.
    Logout {
        provider: AuthProvider,
        account: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthProvider {
    Google,
    Proton,
    Discord,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init { force } => init::run(force)?,
        Command::Status { json } => {
            let line = request_one(&Request::Status).await?;
            if json {
                println!("{line}");
            } else if let Response::Snapshot(snapshot) = serde_json::from_str(&line)? {
                print_status(&snapshot);
            }
        }
        Command::Watch { messages, channels, max_bytes } => watch(messages, channels, max_bytes).await?,
        Command::Ack { account } => ok(Request::Ack { account }).await?,
        Command::MarkRead { account, id } => ok(Request::MarkRead { account, id }).await?,
        Command::MarkUnread { account, id } => ok(Request::MarkUnread { account, id }).await?,
        Command::Trash { account, id } => ok(Request::Trash { account, id }).await?,
        Command::MarkSeen { account, id } => ok(Request::MarkSeen { account, id }).await?,
        Command::MarkUnseen { account, id } => ok(Request::MarkUnseen { account, id }).await?,
        Command::DiscordRead { channel, .. } => ok(Request::DiscordMarkRead { channel }).await?,
        Command::Message { account, id, json, max_chars } => {
            match parse(&request_one(&Request::Message { account, id }).await?)? {
                Response::Message(mut body) => {
                    if max_chars > 0 && body.text.chars().count() > max_chars {
                        body.text = truncate(&body.text, max_chars);
                    }
                    if json {
                        println!("{}", serde_json::to_string(&Response::Message(body))?);
                    } else {
                        print_message(&body);
                    }
                }
                other => anyhow::bail!("unexpected response: {other:?}"),
            }
        }
        Command::Event { id, json } => match parse(&request_one(&Request::Event { id }).await?)? {
            Response::Event(event) => {
                if json {
                    println!("{}", serde_json::to_string(&Response::Event(event))?);
                } else {
                    print_event(&event);
                }
            }
            other => anyhow::bail!("unexpected response: {other:?}"),
        },
        Command::Reload => ok(Request::Reload).await?,
        Command::Auth { provider, account } => {
            auth(provider, account.as_deref(), false).await?;
            reload_if_running().await;
        }
        Command::Logout { provider, account } => {
            auth(provider, account.as_deref(), true).await?;
            reload_if_running().await;
        }
    }
    Ok(())
}

async fn auth(provider: AuthProvider, account: Option<&str>, logout: bool) -> Result<()> {
    let config = Config::load()?;
    let account_id = || {
        account.context("which account? pass the id from the config, e.g. `heron auth google personal`")
    };
    match provider {
        AuthProvider::Google => {
            let account = config.google_account(account_id()?)?;
            if logout {
                return heron_google::logout(account).await;
            }
            let client = config.google_client.as_ref().with_context(|| {
                format!(
                    "add a [google_client] section with client_id and client_secret to {}\n\
                     (Google Cloud console → APIs & Services → Credentials → OAuth client ID → Desktop app)",
                    paths::config_file().display()
                )
            })?;
            heron_google::login(account, client).await
        }
        AuthProvider::Proton => {
            let account = config.proton_account(account_id()?)?;
            if logout { heron_proton::logout(account).await } else { heron_proton::login(account).await }
        }
        AuthProvider::Discord => {
            if logout { heron_discord::logout().await } else { heron_discord::login().await }
        }
    }
}

/// After a login, a running daemon picks up the new credentials.
async fn reload_if_running() {
    if UnixStream::connect(socket_path()).await.is_ok() {
        match ok(Request::Reload).await {
            Ok(()) => eprintln!("herond reloaded"),
            Err(err) => eprintln!("herond reload failed: {err:#}"),
        }
    }
}

type Lines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;

/// Returns the write half too: dropping it would end a watch.
async fn connect(request: &Request) -> Result<(Lines, tokio::net::unix::OwnedWriteHalf)> {
    let path = socket_path();
    let stream = UnixStream::connect(&path)
        .await
        .with_context(|| format!("herond is not running (no socket at {})", path.display()))?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    Ok((BufReader::new(read).lines(), write))
}

async fn request_one(request: &Request) -> Result<String> {
    let (mut lines, _write) = connect(request).await?;
    lines.next_line().await?.context("herond closed the connection")
}

/// Parses a response line, turning an error response into an error.
fn parse(line: &str) -> Result<Response> {
    match serde_json::from_str(line)? {
        Response::Error { message } => anyhow::bail!(message),
        response => Ok(response),
    }
}

/// Sends a request that answers `ok` or an error.
async fn ok(request: Request) -> Result<()> {
    match parse(&request_one(&request).await?)? {
        Response::Ok => Ok(()),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

fn print_message(body: &heron_core::MessageBody) {
    let style = Style::new();
    println!("{}", style.bold(&body.subject));
    println!("{} {}", style.dim("From:"), body.from);
    if !body.to.is_empty() {
        println!("{} {}", style.dim("To:  "), body.to.join(", "));
    }
    if !body.cc.is_empty() {
        println!("{} {}", style.dim("Cc:  "), body.cc.join(", "));
    }
    println!("{} {}\n", style.dim("Date:"), when(body.date, false).trim_end());
    println!("{}", body.text.trim_end());
    if !body.links.is_empty() {
        println!("\n{}", style.bold("Links"));
        for (n, link) in body.links.iter().enumerate() {
            let label = if link.text == link.url { String::new() } else { format!("{}  ", truncate(&link.text, 40)) };
            println!("  [{}] {label}{}", n + 1, style.accent(&link.url));
        }
    }
}

fn print_event(event: &heron_core::CalendarEvent) {
    let style = Style::new();
    println!("{}", style.bold(&event.title));
    println!("{} {}  {}", when(event.start, event.all_day), style.dim("→"), until(event.end, event.all_day).trim_end());
    println!("{} {}", style.dim("Calendar:"), event.calendar);
    if let Some(location) = &event.location {
        println!("{} {location}", style.dim("Where:   "));
    }
    if let Some(url) = &event.meeting_url {
        println!("{} {}", style.dim("Join:    "), style.accent(url));
    }
    if let Some(details) = &event.details {
        if let Some(organizer) = &details.organizer {
            println!("{} {organizer}", style.dim("Organizer:"));
        }
        if !details.attendees.is_empty() {
            println!("{} {}", style.dim("Guests:  "), details.attendees.join(", "));
        }
        if let Some(description) = &details.description {
            println!("\n{}", description.trim_end());
        }
    }
}

async fn watch(messages: Option<usize>, channels: Option<usize>, max_bytes: Option<usize>) -> Result<()> {
    let (mut lines, _write) = connect(&Request::Watch).await?;
    let trimming = messages.is_some() || channels.is_some() || max_bytes.is_some();
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<Response>(&line) {
            Ok(Response::Snapshot(mut snapshot)) if trimming => {
                if let Some(max) = messages {
                    for account in &mut snapshot.mail {
                        account.messages.truncate(max);
                    }
                }
                if let (Some(max), Some(guild)) = (channels, snapshot.discord.as_mut()) {
                    guild.channels.truncate(max);
                }
                let mut line = serde_json::to_string(&Response::Snapshot(snapshot.clone()))?;
                if let Some(max) = max_bytes {
                    while line.len() >= max && shrink(&mut snapshot) {
                        line = serde_json::to_string(&Response::Snapshot(snapshot.clone()))?;
                    }
                }
                println!("{line}");
            }
            _ => println!("{line}"),
        }
    }
    Ok(())
}

/// Drops the last channel's embeds, else the last channel; false when there
/// is nothing left to drop.
fn shrink(snapshot: &mut Snapshot) -> bool {
    let Some(guild) = snapshot.discord.as_mut() else { return false };
    let with_embeds = guild.channels.iter_mut().rev()
        .find(|c| c.messages.iter().any(|m| !m.embeds.is_empty()));
    if let Some(channel) = with_embeds {
        for message in &mut channel.messages {
            if !message.embeds.is_empty() {
                message.embeds.clear();
                if message.content.is_empty() {
                    message.content = "[embed]".into();
                }
            }
        }
        return true;
    }
    guild.channels.pop().is_some()
}

fn print_status(snapshot: &Snapshot) {
    let style = Style::new();

    println!("{}", style.bold(&format!("Mail  {} new · {} unread", snapshot.new, snapshot.unread)));
    for account in &snapshot.mail {
        println!("  {:<12} {:>3} new {:>4} unread", account.id, account.new, account.unread);
        for message in account.messages.iter().filter(|m| m.new).take(5) {
            println!("    {} {}  {}", style.accent("●"), truncate(&message.from, 24), truncate(&message.subject, 60));
        }
    }

    println!("\n{}", style.bold("Agenda"));
    if snapshot.calendar.is_empty() {
        println!("  {}", style.dim("nothing scheduled"));
    }
    for event in snapshot.calendar.iter().take(10) {
        let time = if event.start <= heron_core::now() { until(event.end, event.all_day) } else { when(event.start, event.all_day) };
        println!("  {}  {}  {}", time, truncate(&event.title, 50), style.dim(&event.calendar));
    }

    if let Some(guild) = &snapshot.discord {
        println!(
            "\n{}",
            style.bold(&format!("Discord  {}: {} unread channels · {} mentions", guild.name, guild.unread_channels, guild.mentions))
        );
        for channel in guild.channels.iter().take(8) {
            let mentions = if channel.mentions > 0 { format!(" @{}", channel.mentions) } else { String::new() };
            println!("  #{}{}", channel.name, style.accent(&mentions));
        }
    }

    let problems: Vec<_> = snapshot.sources.iter().filter(|s| s.state != SourceState::Ok).collect();
    if !problems.is_empty() {
        println!("\n{}", style.bold("Sources"));
        for source in problems {
            let hint = match (&source.message, source.state) {
                (Some(message), _) => message.clone(),
                (None, SourceState::NeedsAuth) => "login required, run `heron auth`".to_owned(),
                (None, _) => String::new(),
            };
            println!("  {:<12} {:?} {:?}  {}", source.id, source.kind, source.state, style.dim(&hint));
        }
    }
}

fn when(unix: i64, all_day: bool) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(unix) else { return unix.to_string() };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    let today = jiff::Zoned::now().date();
    let day = match (zoned.date() - today).get_days() {
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        _ => zoned.strftime("%a %d %b").to_string(),
    };
    if all_day { format!("{day:<10} {:>5}", "") } else { format!("{day:<10} {}", zoned.strftime("%H:%M")) }
}

/// "now → 18:00" for an event in progress; the end date when it is not today.
fn until(end: i64, all_day: bool) -> String {
    let Ok(ts) = jiff::Timestamp::from_second(end) else { return "now".into() };
    let zoned = ts.to_zoned(jiff::tz::TimeZone::system());
    // An all-day event ends at the following midnight; name its last day.
    let last = if all_day { zoned.saturating_sub(jiff::SignedDuration::from_secs(1)) } else { zoned };
    let today = jiff::Zoned::now().date();
    let label = match (all_day, last.date() == today) {
        (true, true) => "today".to_owned(),
        (false, true) => last.strftime("%H:%M").to_string(),
        (_, false) => last.strftime("%a %d %b").to_string(),
    };
    format!("{:<16}", format!("now → {label}"))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// ANSI styling only when stdout is a terminal and NO_COLOR is unset.
struct Style(bool);

impl Style {
    fn new() -> Self {
        Self(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.0 { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_owned() }
    }
    fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }
    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }
    fn accent(&self, text: &str) -> String {
        self.wrap("36", text)
    }
}
