//! Per-channel unread message buffers and the plain-text previews shown in
//! the shell: bounded to [`DISCORD_UNREAD_CAP`] ids for counting and the
//! newest [`DISCORD_PREVIEW_MESSAGES`] rendered messages.

use std::collections::{BTreeSet, HashMap, VecDeque};

use heron_core::{DISCORD_PREVIEW_EMBEDS, DISCORD_PREVIEW_MESSAGES, DISCORD_UNREAD_CAP, DiscordEmbed, DiscordMessage};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde_json::Value;

const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
const CONTENT_CHARS: usize = 200;
const EMBED_TITLE_CHARS: usize = 120;
const EMBED_CHARS: usize = 400;
const EMBED_LINES: usize = 10;
const CAP: usize = DISCORD_UNREAD_CAP as usize;

const MESSAGE_USER_JOIN: u64 = 7;
const MESSAGE_CHANNEL_PINNED: u64 = 6;
const MESSAGE_BOOSTS: [u64; 4] = [8, 9, 10, 11];

/// Unread messages of one channel: ids newer than the read state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Buffer {
    /// The newest unread message ids, at most [`DISCORD_UNREAD_CAP`].
    ids: BTreeSet<u64>,
    /// Unread messages older than every id in `ids` exist.
    more: bool,
    /// The newest unread messages, oldest first.
    previews: VecDeque<DiscordMessage>,
    /// Every unread message up to this id is accounted for in `ids`/`more`.
    pub through: Option<u64>,
}

impl Buffer {
    /// Adds an unread message. `preview` may be `None` for messages that
    /// are only counted (older history); it is ignored when the message is
    /// not among the newest few.
    pub fn insert(&mut self, id: u64, preview: Option<DiscordMessage>) {
        self.ids.insert(id);
        if self.ids.len() > CAP {
            self.ids.pop_first();
            self.more = true;
        }
        if !self.ids.contains(&id) {
            return;
        }
        let Some(preview) = preview else { return };
        let at = self.previews.partition_point(|p| preview_id(p) < id);
        if self.previews.get(at).is_some_and(|p| preview_id(p) == id) {
            self.previews[at] = preview;
        } else if at > 0 || self.previews.len() < DISCORD_PREVIEW_MESSAGES {
            self.previews.insert(at, preview);
            while self.previews.len() > DISCORD_PREVIEW_MESSAGES {
                self.previews.pop_front();
            }
        }
    }

    /// Everything up to `acked` was read.
    pub fn ack(&mut self, acked: u64) {
        // Unread messages beyond the buffer are older than its oldest id.
        if self.ids.first().is_none_or(|first| acked >= *first) {
            self.more = false;
        }
        self.ids.retain(|id| *id > acked);
        self.previews.retain(|p| preview_id(p) > acked);
    }

    pub fn remove(&mut self, id: u64) -> bool {
        let before = self.previews.len();
        self.previews.retain(|p| preview_id(p) != id);
        self.ids.remove(&id) || before != self.previews.len()
    }

    /// Replaces a previewed message in place; false when it is not previewed.
    pub fn update(&mut self, id: u64, f: impl FnOnce(&mut DiscordMessage)) -> bool {
        match self.previews.iter_mut().find(|p| preview_id(p) == id) {
            Some(preview) => {
                f(preview);
                true
            }
            None => false,
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.ids.contains(&id)
    }

    pub fn mark_more(&mut self) {
        self.more = true;
    }

    /// (unread count, more beyond the cap).
    pub fn count(&self) -> (u32, bool) {
        (self.ids.len() as u32, self.more)
    }

    pub fn previews(&self) -> Vec<DiscordMessage> {
        self.previews.iter().cloned().collect()
    }
}

fn preview_id(preview: &DiscordMessage) -> u64 {
    preview.id.parse().unwrap_or(0)
}

pub trait ChannelNames {
    fn channel_name(&self, id: u64) -> Option<String>;
}

impl<F: Fn(u64) -> Option<String>> ChannelNames for F {
    fn channel_name(&self, id: u64) -> Option<String> {
        self(id)
    }
}

/// Names for resolving `<@id>`, `<@&id>` and `<#id>`.
pub struct Names<'a> {
    pub nicks: &'a HashMap<u64, String>,
    pub roles: &'a HashMap<u64, String>,
    pub channels: &'a dyn ChannelNames,
}

/// Display name: guild nick, then global name, then username.
pub fn display_name(user: &Value, member: Option<&Value>, nicks: &HashMap<u64, String>) -> String {
    member.and_then(|m| m.get("nick")).and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| snowflake(user.get("id")).and_then(|id| nicks.get(&id).cloned()))
        .or_else(|| user.get("global_name").and_then(Value::as_str).map(str::to_owned))
        .or_else(|| user.get("username").and_then(Value::as_str).map(str::to_owned))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The preview of one message payload (gateway or REST shape).
pub fn preview(d: &Value, mentions_me: bool, names: &Names) -> Option<DiscordMessage> {
    let id = snowflake(d.get("id"))?;
    let author = d.get("author").map_or_else(|| "unknown".to_owned(), |a| display_name(a, d.get("member"), names.nicks));
    let timestamp = d.get("timestamp").and_then(Value::as_str)
        .and_then(|t| t.parse::<Timestamp>().ok())
        .map_or_else(|| ((id >> 22) as i64 + DISCORD_EPOCH_MS) / 1000, |t| t.as_second());
    let embeds = embeds(d, names);
    Some(DiscordMessage { id: id.to_string(), author, content: content(d, names), timestamp, mentions_me, embeds })
}

/// Plain-text content, mentions resolved, whitespace collapsed, at most
/// [`CONTENT_CHARS`] characters; a placeholder when there is no text. Empty
/// when the message is only rich embeds, which [`embeds`] renders.
pub fn content(d: &Value, names: &Names) -> String {
    let raw = d.get("content").and_then(Value::as_str).unwrap_or_default();
    let text = resolve(raw, d, names);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = if text.is_empty() { placeholder(d, !embeds(d, names).is_empty()) } else { text };
    truncate(&text, CONTENT_CHARS)
}

/// The message's rich embeds (bots, webhooks) with any text, as plain text.
/// Link previews (`article`, `image`, `video`, ...) only repeat the content.
pub fn embeds(d: &Value, names: &Names) -> Vec<DiscordEmbed> {
    let list = d.get("embeds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter(|e| e.get("type").and_then(Value::as_str).is_none_or(|t| t == "rich"))
        .filter_map(|e| embed(e, d, names))
        .take(DISCORD_PREVIEW_EMBEDS)
        .collect()
}

fn embed(e: &Value, d: &Value, names: &Names) -> Option<DiscordEmbed> {
    let str = |v: Option<&Value>| v.and_then(Value::as_str).map(|s| plain(&resolve(s, d, names))).filter(|s| !s.is_empty());
    let title = str(e.get("title")).or_else(|| str(e.get("author").and_then(|a| a.get("name"))));
    let mut lines: Vec<String> = Vec::new();
    if let Some(description) = str(e.get("description")) {
        lines.extend(description.lines().map(str::to_owned));
    }
    for field in e.get("fields").and_then(Value::as_array).into_iter().flatten() {
        match (str(field.get("name")), str(field.get("value"))) {
            (Some(name), Some(value)) => lines.push(format!("{name}: {}", value.lines().collect::<Vec<_>>().join(" "))),
            (name, value) => lines.extend(name.or(value)),
        }
    }
    if let Some(footer) = str(e.get("footer").and_then(|f| f.get("text"))) {
        lines.push(footer);
    }
    lines.retain(|line| !line.trim().is_empty());
    let more = lines.len() > EMBED_LINES;
    lines.truncate(EMBED_LINES);
    let mut text = truncate(&lines.join("\n"), EMBED_CHARS);
    if more && !text.ends_with('…') {
        text.push('…');
    }
    if title.is_none() && text.is_empty() {
        return None;
    }
    Some(DiscordEmbed {
        title: title.map(|t| truncate(&t, EMBED_TITLE_CHARS)),
        text,
        color: e.get("color").and_then(Value::as_u64).filter(|c| *c != 0).map(|c| format!("#{c:06x}")),
        url: e.get("url").and_then(Value::as_str).filter(|u| u.starts_with("https://") || u.starts_with("http://")).map(str::to_owned),
    })
}

/// Discord markdown reduced to plain text, line by line: `[text](url)` keeps
/// the text, and bold, underline, strikethrough, code and heading/quote markers
/// go. Single `*` and `_` stay, being common in names and paths.
fn plain(text: &str) -> String {
    text.lines()
        .map(|line| {
            let line = line.trim_end();
            let line = line.trim_start_matches("> ").trim_start_matches(">>> ");
            let line = line.trim_start_matches('#').trim_start();
            let line = unlink(line);
            let line = line.replace("```", "").replace("**", "").replace("__", "").replace("~~", "").replace("||", "").replace('`', "");
            line.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// `[text](url)` → `text`.
fn unlink(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let link = after.find("](").and_then(|mid| {
            let close = after[mid + 2..].find(')')?;
            Some((mid, mid + 2 + close))
        });
        match link {
            Some((mid, close)) if !after[..mid].contains('[') => {
                out.push_str(&rest[..open]);
                out.push_str(&after[..mid]);
                rest = &after[close + 1..];
            }
            _ => {
                out.push_str(&rest[..=open]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn placeholder(d: &Value, rich: bool) -> String {
    let kind = d.get("type").and_then(Value::as_u64).unwrap_or(0);
    if kind == MESSAGE_USER_JOIN {
        return "joined the server".to_owned();
    }
    if kind == MESSAGE_CHANNEL_PINNED {
        return "pinned a message".to_owned();
    }
    if MESSAGE_BOOSTS.contains(&kind) {
        return "boosted the server".to_owned();
    }
    let present = |key: &str| d.get(key).and_then(Value::as_array).is_some_and(|a| !a.is_empty());
    let mut parts: Vec<&str> = Vec::new();
    let attachments = d.get("attachments").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    if attachments.iter().any(is_image) {
        parts.push("[image]");
    }
    if attachments.iter().any(|a| !is_image(a)) {
        parts.push("[file]");
    }
    if present("embeds") && !rich {
        parts.push("[embed]");
    }
    if present("sticker_items") || present("stickers") {
        parts.push("[sticker]");
    }
    if d.get("poll").is_some_and(|p| !p.is_null()) {
        parts.push("[poll]");
    }
    if present("message_snapshots") {
        parts.push("[forwarded]");
    }
    if parts.is_empty() && !rich { "[message]".to_owned() } else { parts.join(" ") }
}

fn is_image(attachment: &Value) -> bool {
    if let Some(kind) = attachment.get("content_type").and_then(Value::as_str) {
        return kind.starts_with("image/");
    }
    let name = attachment.get("filename").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".avif"].iter().any(|ext| name.ends_with(ext))
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some(_) => {
            let mut cut: String = text.chars().take(max - 1).collect();
            cut.truncate(cut.trim_end().len());
            cut.push('…');
            cut
        }
    }
}

/// Replaces `<@id>`, `<@!id>`, `<@&id>`, `<#id>`, `<:name:id>`,
/// `<a:name:id>` and `<t:unix(:style)>` tokens.
fn resolve(raw: &str, d: &Value, names: &Names) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let token = after.find('>').map(|end| &after[..end]).filter(|t| !t.contains('<'));
        match token.and_then(|t| replacement(t, d, names)) {
            Some(text) => {
                out.push_str(&text);
                rest = &after[token.unwrap_or_default().len() + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn replacement(token: &str, d: &Value, names: &Names) -> Option<String> {
    let id = |s: &str| s.parse::<u64>().ok().filter(|_| s.bytes().all(|b| b.is_ascii_digit()));
    if let Some(role) = token.strip_prefix("@&") {
        let role = id(role)?;
        return Some(format!("@{}", names.roles.get(&role).map_or("deleted-role", String::as_str)));
    }
    if let Some(user) = token.strip_prefix('@') {
        let user = id(user.strip_prefix('!').unwrap_or(user))?;
        let mentioned = d.get("mentions").and_then(Value::as_array).into_iter().flatten()
            .find(|m| snowflake(m.get("id")) == Some(user));
        let name = match mentioned {
            Some(m) => display_name(m, m.get("member"), names.nicks),
            None => names.nicks.get(&user).cloned().unwrap_or_else(|| "unknown-user".to_owned()),
        };
        return Some(format!("@{name}"));
    }
    if let Some(channel) = token.strip_prefix('#') {
        let channel = id(channel)?;
        return Some(format!("#{}", names.channels.channel_name(channel).unwrap_or_else(|| "unknown".to_owned())));
    }
    if let Some(time) = token.strip_prefix("t:") {
        let seconds = time.split(':').next()?.parse::<i64>().ok()?;
        let zoned = Timestamp::from_second(seconds).ok()?.to_zoned(TimeZone::system());
        return Some(zoned.strftime("%Y-%m-%d %H:%M").to_string());
    }
    let emoji = token.strip_prefix("a:").or_else(|| token.strip_prefix(':'))?;
    let (name, emoji_id) = emoji.split_once(':')?;
    id(emoji_id)?;
    (!name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')).then(|| format!(":{name}:"))
}

fn snowflake(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_str().and_then(|s| s.parse().ok()).or_else(|| value.as_u64()).filter(|id| *id != 0)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn msg(id: u64) -> DiscordMessage {
        DiscordMessage { id: id.to_string(), author: "a".into(), content: id.to_string(), timestamp: 0, mentions_me: false, embeds: Vec::new() }
    }

    fn ids(buffer: &Buffer) -> Vec<u64> {
        buffer.previews.iter().map(preview_id).collect()
    }

    #[test]
    fn buffer_caps_and_keeps_newest_previews() {
        let mut buffer = Buffer::default();
        for id in (1..=150).rev() {
            buffer.insert(id, Some(msg(id)));
        }
        assert_eq!(buffer.count(), (100, true));
        assert_eq!(ids(&buffer), [148, 149, 150]);
        // An older message does not displace the newest previews.
        buffer.insert(120, Some(msg(120)));
        assert_eq!(ids(&buffer), [148, 149, 150]);
        // Acking below the buffer keeps `more`; at or above clears it.
        buffer.ack(10);
        assert_eq!(buffer.count(), (100, true));
        buffer.ack(149);
        assert_eq!(buffer.count(), (1, false));
        assert_eq!(ids(&buffer), [150]);
    }

    #[test]
    fn buffer_remove_and_update() {
        let mut buffer = Buffer::default();
        for id in [5, 6, 7] {
            buffer.insert(id, Some(msg(id)));
        }
        assert!(buffer.update(6, |m| m.content = "edited".into()));
        assert!(!buffer.update(99, |_| {}));
        assert_eq!(buffer.previews[1].content, "edited");
        assert!(buffer.remove(7));
        assert!(!buffer.remove(7));
        assert_eq!(buffer.count(), (2, false));
        assert_eq!(ids(&buffer), [5, 6]);
        // Re-inserting an id replaces its preview without double counting.
        buffer.insert(6, Some(msg(6)));
        assert_eq!(buffer.count(), (2, false));
        assert_eq!(buffer.previews[1].content, "6");
    }

    fn names_with<'a>(nicks: &'a HashMap<u64, String>, roles: &'a HashMap<u64, String>, channels: &'a dyn ChannelNames) -> Names<'a> {
        Names { nicks, roles, channels }
    }

    #[test]
    fn content_resolution() {
        let nicks = HashMap::from([(42, "Nicky".to_owned())]);
        let roles = HashMap::from([(2000, "mod".to_owned())]);
        let channels = |id: u64| (id == 3001).then(|| "general".to_owned());
        let names = names_with(&nicks, &roles, &channels);
        let d = json!({
            "id": "10", "content": "hi <@8> and <@!42>, <@&2000> <@&9> see <#3001> <#1> <:pog:123> <a:dance:456> <@99>\n\nok < not a tag >",
            "mentions": [{"id": "8", "username": "friend", "global_name": "Friend", "member": {"nick": "Buddy"}}],
        });
        assert_eq!(
            content(&d, &names),
            "hi @Buddy and @Nicky, @mod @deleted-role see #general #unknown :pog: :dance: @unknown-user ok < not a tag >"
        );
    }

    #[test]
    fn rich_embeds() {
        let nicks = HashMap::from([(7, "Owner".to_owned())]);
        let roles = HashMap::new();
        let channels = |_| None;
        let names = names_with(&nicks, &roles, &channels);
        let d = json!({"content": "", "embeds": [
            {"type": "rich", "title": "[FIRING] CronJobLastRunFailed", "color": 15158332,
             "url": "https://alerts.example/1",
             "description": "**warning** adv: CronJob `adv/device-sync` did not succeed\n\n> see [runbook](https://x.example/rb) <@7>",
             "fields": [{"name": "Severity", "value": "warning"}, {"name": "", "value": "only value"}],
             "footer": {"text": "Alertmanager"}},
            {"type": "article", "title": "Link preview", "description": "ignored"},
            {"type": "rich"},
        ]});
        let embeds = embeds(&d, &names);
        assert_eq!(embeds, [DiscordEmbed {
            title: Some("[FIRING] CronJobLastRunFailed".into()),
            text: "warning adv: CronJob adv/device-sync did not succeed\nsee runbook @Owner\nSeverity: warning\nonly value\nAlertmanager".into(),
            color: Some("#e74c3c".into()),
            url: Some("https://alerts.example/1".into()),
        }]);
        assert_eq!(content(&d, &names), "");

        let many: Vec<String> = (0..30).map(|n| format!("line {n}")).collect();
        let long = json!({"embeds": [{"description": many.join("\n")}]});
        let text = &super::embeds(&long, &names)[0].text;
        assert_eq!(text.lines().count(), EMBED_LINES);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn placeholders_and_truncation() {
        let nicks = HashMap::new();
        let roles = HashMap::new();
        let channels = |_| None;
        let names = names_with(&nicks, &roles, &channels);
        let attach = json!({"content": "", "attachments": [{"filename": "a.PNG"}, {"filename": "b.zip", "content_type": "application/zip"}]});
        assert_eq!(content(&attach, &names), "[image] [file]");
        assert_eq!(content(&json!({"content": "", "embeds": [{}]}), &names), "[embed]");
        assert_eq!(content(&json!({"content": "", "embeds": [{"title": "Alert"}]}), &names), "");
        assert_eq!(content(&json!({"content": " ", "sticker_items": [{"id": "1"}]}), &names), "[sticker]");
        assert_eq!(content(&json!({"type": 7, "content": ""}), &names), "joined the server");
        assert_eq!(content(&json!({"content": ""}), &names), "[message]");

        let long = "é".repeat(300);
        let cut = content(&json!({"content": long}), &names);
        assert_eq!(cut.chars().count(), 200);
        assert!(cut.ends_with('…'));
        assert_eq!(content(&json!({"content": "x".repeat(200)}), &names).chars().count(), 200);
        assert!(!content(&json!({"content": "x".repeat(200)}), &names).ends_with('…'));
    }

    #[test]
    fn author_names_and_timestamps() {
        let nicks = HashMap::from([(9, "Cached".to_owned())]);
        let roles = HashMap::new();
        let channels = |_| None;
        let names = names_with(&nicks, &roles, &channels);
        let base = json!({"id": "1200000000000000000", "content": "x", "timestamp": "2026-10-22T12:00:00.000000+00:00"});
        let with = |author: Value, member: Value| {
            let mut d = base.clone();
            d["author"] = author;
            d["member"] = member;
            preview(&d, true, &names).unwrap()
        };
        let p = with(json!({"id": "8", "username": "u", "global_name": "G"}), json!({"nick": "N"}));
        assert_eq!((p.author.as_str(), p.timestamp, p.mentions_me), ("N", 1_792_670_400, true));
        assert_eq!(with(json!({"id": "8", "username": "u", "global_name": "G"}), json!({"nick": null})).author, "G");
        assert_eq!(with(json!({"id": "8", "username": "u", "global_name": null}), Value::Null).author, "u");
        assert_eq!(with(json!({"id": "9", "username": "u"}), Value::Null).author, "Cached");
        // Without a timestamp the snowflake's time is used.
        let p = preview(&json!({"id": "1200000000000000000", "content": "x"}), false, &names).unwrap();
        assert_eq!(p.timestamp, (1_200_000_000_000_000_000i64 >> 22) / 1000 + DISCORD_EPOCH_MS / 1000);
    }
}
