//! Local model of the one configured guild, built from READY and kept current
//! from dispatch events, and the [`DiscordGuild`] computed from it.
//!
//! Payload shapes, read-state rules and notification-setting handling are
//! adapted from concord (github.com/chojs23/concord, GPL-3.0),
//! `src/discord/gateway/parser/*.rs`, `src/discord/read/state.rs` and
//! `src/discord/notification/state.rs`, and from discord-userdoccers'
//! read-state documentation.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use heron_core::{DISCORD_UNREAD_CAP, DiscordChannel, DiscordGuild, DiscordMessage};
use jiff::{SignedDuration, Timestamp};
use serde_json::Value;
use tracing::debug;

use crate::history::{self, Fetched};
use crate::messages::{self, Buffer, Names};
use crate::permissions::{Member, Overwrite, OverwriteKind};

const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;

const CHANNEL_TEXT: u64 = 0;
const CHANNEL_ANNOUNCEMENT: u64 = 5;
const MESSAGE_POLL_RESULT: u64 = 46;
const RELATIONSHIP_BLOCKED: u64 = 2;

/// User notification flag: every message in an "all messages" channel counts
/// as a (low importance) mention.
const USER_MENTION_ON_ALL_MESSAGES: u64 = 1 << 5;
const CHANNEL_UNREADS_ONLY_MENTIONS: u64 = 1 << 9;
const CHANNEL_UNREADS_ALL_MESSAGES: u64 = 1 << 10;
const CHANNEL_OPT_IN_ENABLED: u64 = 1 << 12;
const GUILD_UNREADS_ALL_MESSAGES: u64 = 1 << 11;
const GUILD_UNREADS_ONLY_MENTIONS: u64 = 1 << 12;
const GUILD_OPT_IN_CHANNELS_OFF: u64 = 1 << 13;
const GUILD_OPT_IN_CHANNELS_ON: u64 = 1 << 14;

const NOTIFY_ALL_MESSAGES: u64 = 0;
const NOTIFY_PARENT_DEFAULT: u64 = 3;

/// A failed history fetch is retried after this long.
const FETCH_RETRY: SignedDuration = SignedDuration::from_mins(5);
/// Nicknames remembered from message events, for REST history authors.
const MAX_NICKS: usize = 5000;

#[derive(Debug, Clone, PartialEq)]
struct Channel {
    id: u64,
    kind: u64,
    name: String,
    parent: Option<u64>,
    position: i64,
    last_message_id: Option<u64>,
    overwrites: Vec<Overwrite>,
}

impl Channel {
    fn is_text(&self) -> bool {
        matches!(self.kind, CHANNEL_TEXT | CHANNEL_ANNOUNCEMENT)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ReadState {
    last_acked: Option<u64>,
    mentions: u32,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Mute {
    muted: bool,
    /// `None` while muted means indefinitely.
    until: Option<Timestamp>,
}

impl Mute {
    fn parse(value: &Value) -> Self {
        let until = value.pointer("/mute_config/end_time")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Timestamp>().ok());
        Self { muted: value.get("muted").and_then(Value::as_bool).unwrap_or(false), until }
    }

    fn active(&self, now: Timestamp) -> bool {
        self.muted && self.until.is_none_or(|until| until > now)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Override {
    mute: Mute,
    flags: u64,
    message_notifications: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Settings {
    mute: Mute,
    suppress_everyone: bool,
    suppress_roles: bool,
    flags: u64,
    message_notifications: Option<u64>,
    overrides: HashMap<u64, Override>,
}

impl Settings {
    fn parse(value: &Value) -> Self {
        let parse_override = |v: &Value| Override {
            mute: Mute::parse(v),
            flags: v.get("flags").and_then(Value::as_u64).unwrap_or(0),
            message_notifications: v.get("message_notifications").and_then(Value::as_u64),
        };
        // An array in READY and updates; a map keyed by channel id in some payloads.
        let overrides = match value.get("channel_overrides") {
            Some(Value::Array(items)) => items.iter()
                .filter_map(|v| Some((snowflake(v.get("channel_id"))?, parse_override(v))))
                .collect(),
            Some(Value::Object(map)) => map.iter()
                .filter_map(|(k, v)| Some((k.parse().ok()?, parse_override(v))))
                .collect(),
            _ => HashMap::new(),
        };
        Self {
            mute: Mute::parse(value),
            suppress_everyone: value.get("suppress_everyone").and_then(Value::as_bool).unwrap_or(false),
            suppress_roles: value.get("suppress_roles").and_then(Value::as_bool).unwrap_or(false),
            flags: value.get("flags").and_then(Value::as_u64).unwrap_or(0),
            message_notifications: value.get("message_notifications").and_then(Value::as_u64),
            overrides,
        }
    }
}

/// Everything known about the configured guild.
#[derive(Debug, Clone, Default)]
pub struct GuildState {
    guild_id: u64,
    user_id: Option<u64>,
    /// The guild was in READY or a later GUILD_CREATE and is available.
    present: bool,
    name: String,
    owner_id: Option<u64>,
    default_notifications: Option<u64>,
    /// Snowflake of the moment the user joined; channels without a read
    /// state count as read up to here.
    joined: Option<u64>,
    roles: HashMap<u64, u64>,
    member_roles: Vec<u64>,
    channels: HashMap<u64, Channel>,
    read_states: HashMap<u64, ReadState>,
    settings: Settings,
    blocked: HashSet<u64>,
    user_flags: u64,
    role_names: HashMap<u64, String>,
    /// Guild nicknames seen in message events; REST history has no members.
    nicks: HashMap<u64, String>,
    /// Unread messages per channel.
    buffers: HashMap<u64, Buffer>,
    /// History requested and not yet loaded.
    pending: HashSet<u64>,
    /// When a history fetch last failed.
    failed: HashMap<u64, Timestamp>,
}

/// A channel in the published list.
struct Listed<'a> {
    channel: &'a Channel,
    mentions: u32,
    /// Has messages newer than the read state.
    unread: bool,
    /// Counts as unread (not muted, not "only mentions", opted in).
    shown: bool,
    key: (Reverse<u32>, i64, i64, u64),
}

impl GuildState {
    pub fn new(guild_id: u64) -> Self {
        Self { guild_id, ..Self::default() }
    }

    /// Whether READY (or GUILD_CREATE) delivered the guild.
    pub fn present(&self) -> bool {
        self.present
    }

    /// Applies one dispatch; returns whether anything relevant changed.
    pub fn apply(&mut self, event: &str, d: &Value) -> bool {
        match event {
            "READY" => {
                self.ready(d);
                true
            }
            "READY_SUPPLEMENTAL" => self.supplemental(d),
            "GUILD_CREATE" if self.is_ours(d.get("id")) => {
                self.load_guild(d);
                true
            }
            "GUILD_UPDATE" if self.is_ours(d.get("id")) => {
                if let Some(name) = guild_field(d, "name").and_then(Value::as_str) {
                    self.name = name.to_owned();
                }
                if let Some(owner) = snowflake(guild_field(d, "owner_id")) {
                    self.owner_id = Some(owner);
                }
                true
            }
            "GUILD_DELETE" if self.is_ours(d.get("id")) => {
                self.present = false;
                self.channels.clear();
                true
            }
            "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" if self.is_ours(d.get("guild_id")) => {
                let Some(role) = d.get("role") else { return false };
                if let (Some(id), Some(name)) = (snowflake(role.get("id")), role.get("name").and_then(Value::as_str)) {
                    self.role_names.insert(id, name.to_owned());
                }
                parse_role(role).map(|(id, perms)| self.roles.insert(id, perms)).is_some()
            }
            "GUILD_ROLE_DELETE" if self.is_ours(d.get("guild_id")) => {
                let Some(id) = snowflake(d.get("role_id")) else { return false };
                self.role_names.remove(&id);
                self.roles.remove(&id).is_some()
            }
            "GUILD_MEMBER_UPDATE" if self.is_ours(d.get("guild_id")) => {
                let user = snowflake(d.pointer("/user/id"));
                if user.is_none_or(|id| Some(id) != self.user_id) {
                    if let Some(user) = user.filter(|id| self.nicks.contains_key(id)) {
                        self.remember_nick(user, d);
                    }
                    return false;
                }
                self.member_roles = snowflakes(d.get("roles"));
                true
            }
            "CHANNEL_CREATE" | "CHANNEL_UPDATE" if self.is_ours(d.get("guild_id")) => {
                let Some(mut channel) = parse_channel(d) else { return false };
                if let Some(old) = self.channels.get(&channel.id) {
                    channel.last_message_id = channel.last_message_id.max(old.last_message_id);
                }
                self.channels.insert(channel.id, channel);
                true
            }
            "CHANNEL_DELETE" if self.is_ours(d.get("guild_id")) => {
                let Some(id) = snowflake(d.get("id")) else { return false };
                self.read_states.remove(&id);
                self.buffers.remove(&id);
                self.channels.remove(&id).is_some()
            }
            "CHANNEL_UNREAD_UPDATE" if self.is_ours(d.get("guild_id")) => {
                let mut changed = false;
                for update in d.get("channel_unread_updates").and_then(Value::as_array).into_iter().flatten() {
                    let latest = snowflake(update.get("last_message_id"));
                    if let Some(channel) = snowflake(update.get("id")).and_then(|id| self.channels.get_mut(&id))
                        && latest > channel.last_message_id
                    {
                        channel.last_message_id = latest;
                        changed = true;
                    }
                }
                changed
            }
            "MESSAGE_CREATE" => self.message_create(d),
            "MESSAGE_UPDATE" => self.message_update(d),
            "MESSAGE_DELETE" if self.is_ours(d.get("guild_id")) => {
                let (Some(channel), Some(id)) = (snowflake(d.get("channel_id")), snowflake(d.get("id"))) else {
                    return false;
                };
                self.buffers.get_mut(&channel).is_some_and(|b| b.remove(id))
            }
            "MESSAGE_DELETE_BULK" if self.is_ours(d.get("guild_id")) => {
                let Some(buffer) = snowflake(d.get("channel_id")).and_then(|c| self.buffers.get_mut(&c)) else {
                    return false;
                };
                snowflakes(d.get("ids")).into_iter().fold(false, |changed, id| buffer.remove(id) | changed)
            }
            "MESSAGE_ACK" => self.message_ack(d),
            "USER_GUILD_SETTINGS_UPDATE" if self.is_ours(d.get("guild_id")) => {
                self.settings = Settings::parse(d);
                true
            }
            "USER_NOTIFICATION_SETTINGS_UPDATE" => {
                self.user_flags = d.get("flags").and_then(Value::as_u64).unwrap_or(self.user_flags);
                false
            }
            "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" => {
                let id = snowflake(d.get("id")).or_else(|| snowflake(d.pointer("/user/id")));
                if let Some(id) = id {
                    if d.get("type").and_then(Value::as_u64) == Some(RELATIONSHIP_BLOCKED) {
                        self.blocked.insert(id);
                    } else {
                        self.blocked.remove(&id);
                    }
                }
                false
            }
            "RELATIONSHIP_REMOVE" => {
                if let Some(id) = snowflake(d.get("id")) {
                    self.blocked.remove(&id);
                }
                false
            }
            _ => false,
        }
    }

    fn is_ours(&self, id: Option<&Value>) -> bool {
        snowflake(id) == Some(self.guild_id)
    }

    fn ready(&mut self, d: &Value) {
        *self = Self::new(self.guild_id);
        self.user_id = snowflake(d.pointer("/user/id"));
        self.user_flags = d.pointer("/notification_settings/flags").and_then(Value::as_u64).unwrap_or(0);
        self.blocked = d.get("relationships").and_then(Value::as_array).into_iter().flatten()
            .filter(|r| r.get("type").and_then(Value::as_u64) == Some(RELATIONSHIP_BLOCKED))
            .filter_map(|r| snowflake(r.get("id")))
            .collect();

        let guilds = d.get("guilds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        if let Some((index, guild)) = guilds.iter().enumerate().find(|(_, g)| self.is_ours(g.get("id"))) {
            if guild.get("unavailable").and_then(Value::as_bool) != Some(true) {
                self.load_guild(guild);
            }
            // `merged_members` is parallel to `guilds`.
            if let Some(members) = d.pointer(&format!("/merged_members/{index}")).and_then(Value::as_array) {
                self.find_member(members);
            }
        }

        // VERSIONED_READ_STATES wraps entries as `{entries, version, partial}`.
        let read_state = d.get("read_state");
        let entries = read_state.and_then(|r| r.get("entries")).or(read_state).and_then(Value::as_array);
        for entry in entries.into_iter().flatten() {
            if entry.get("read_state_type").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(id) = snowflake(entry.get("id")) {
                let state = ReadState {
                    last_acked: snowflake(entry.get("last_message_id")),
                    mentions: count(entry.get("mention_count")),
                };
                self.read_states.insert(id, state);
            }
        }

        let settings = d.get("user_guild_settings");
        let entries = settings.and_then(|s| s.get("entries")).or(settings).and_then(Value::as_array);
        if let Some(entry) = entries.into_iter().flatten().find(|e| self.is_ours(e.get("guild_id"))) {
            self.settings = Settings::parse(entry);
        }
    }

    fn supplemental(&mut self, d: &Value) -> bool {
        let guilds = d.get("guilds").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let Some(index) = guilds.iter().position(|g| self.is_ours(g.get("id"))) else { return false };
        let Some(members) = d.pointer(&format!("/merged_members/{index}")).and_then(Value::as_array) else {
            return false;
        };
        let before = self.member_roles.clone();
        self.find_member(members);
        before != self.member_roles
    }

    /// Takes the guild's metadata, roles and channels from a gateway guild object.
    fn load_guild(&mut self, g: &Value) {
        self.present = true;
        self.name = guild_field(g, "name").and_then(Value::as_str).unwrap_or("Discord").to_owned();
        self.owner_id = snowflake(guild_field(g, "owner_id"));
        self.default_notifications = guild_field(g, "default_message_notifications").and_then(Value::as_u64);
        if let Some(joined) = g.get("joined_at").and_then(Value::as_str).and_then(timestamp_snowflake) {
            self.joined = Some(joined);
        }
        let roles = g.get("roles").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        self.roles = roles.iter().filter_map(parse_role).collect();
        self.role_names = roles.iter()
            .filter_map(|r| Some((snowflake(r.get("id"))?, r.get("name")?.as_str()?.to_owned())))
            .collect();
        let old = std::mem::take(&mut self.channels);
        self.channels = g.get("channels").and_then(Value::as_array).into_iter().flatten()
            .filter_map(parse_channel)
            .map(|mut c| {
                if let Some(prev) = old.get(&c.id) {
                    c.last_message_id = c.last_message_id.max(prev.last_message_id);
                }
                (c.id, c)
            })
            .collect();
        if let Some(members) = g.get("members").and_then(Value::as_array) {
            self.find_member(members);
        }
    }

    fn find_member(&mut self, members: &[Value]) {
        let me = members.iter().find(|m| {
            let id = snowflake(m.get("user_id")).or_else(|| snowflake(m.pointer("/user/id")));
            id.is_some() && id == self.user_id
        });
        if let Some(me) = me {
            self.member_roles = snowflakes(me.get("roles"));
            if let Some(joined) = me.get("joined_at").and_then(Value::as_str).and_then(timestamp_snowflake) {
                self.joined = Some(joined);
            }
        }
    }

    fn message_create(&mut self, d: &Value) -> bool {
        if !self.is_ours(d.get("guild_id")) {
            return false;
        }
        let (Some(channel_id), Some(message_id)) = (snowflake(d.get("channel_id")), snowflake(d.get("id"))) else {
            return false;
        };
        if !self.channels.contains_key(&channel_id) {
            return false;
        }
        // Whether every unread message so far is buffered, before this one
        // moves `last_message_id`.
        let complete = self.complete(channel_id);
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.last_message_id = channel.last_message_id.max(Some(message_id));
        }

        let author = snowflake(d.pointer("/author/id"));
        if let (Some(author), Some(member)) = (author, d.get("member")) {
            self.remember_nick(author, member);
        }
        for mention in d.get("mentions").and_then(Value::as_array).into_iter().flatten() {
            if let (Some(user), Some(member)) = (snowflake(mention.get("id")), mention.get("member")) {
                self.remember_nick(user, member);
            }
        }

        if author.is_some() && author == self.user_id {
            // Sending a message acknowledges the channel.
            if d.get("type").and_then(Value::as_u64) != Some(MESSAGE_POLL_RESULT) {
                let state = self.read_states.entry(channel_id).or_default();
                state.last_acked = state.last_acked.max(Some(message_id));
                state.mentions = 0;
                if let Some(buffer) = self.buffers.get_mut(&channel_id) {
                    buffer.ack(message_id);
                }
            }
            return true;
        }
        if author.is_some_and(|a| self.blocked.contains(&a)) {
            if complete {
                self.buffers.entry(channel_id).or_default().through = Some(message_id);
            }
            return true;
        }
        if self.acked(channel_id).is_some_and(|acked| acked >= message_id) {
            return true;
        }
        let mentions_me = self.mentions_me(d);
        if mentions_me || self.low_importance_mention(channel_id, Timestamp::now()) {
            let state = self.read_states.entry(channel_id).or_default();
            state.mentions = state.mentions.saturating_add(1);
        }
        let preview = self.preview(d, mentions_me);
        let buffer = self.buffers.entry(channel_id).or_default();
        buffer.insert(message_id, preview);
        if complete {
            buffer.through = buffer.through.max(Some(message_id));
        }
        true
    }

    /// Edits re-render a previewed message; other edits change nothing shown.
    fn message_update(&mut self, d: &Value) -> bool {
        // Partial updates carry `content`, `embeds`, or both.
        if !self.is_ours(d.get("guild_id")) || (d.get("content").is_none() && d.get("embeds").is_none()) {
            return false;
        }
        let (Some(channel_id), Some(message_id)) = (snowflake(d.get("channel_id")), snowflake(d.get("id"))) else {
            return false;
        };
        if !self.buffers.get(&channel_id).is_some_and(|b| b.contains(message_id)) {
            return false;
        }
        let mentions_me = d.get("mentions").map(|_| self.mentions_me(d));
        let names = self.names();
        let content = d.get("content").map(|_| messages::content(d, &names));
        let embeds = d.get("embeds").map(|_| messages::embeds(d, &names));
        let Some(buffer) = self.buffers.get_mut(&channel_id) else { return false };
        let mut changed = false;
        buffer.update(message_id, |m| {
            let before = m.clone();
            if let Some(content) = content {
                m.content = content;
            }
            if let Some(embeds) = embeds {
                m.embeds = embeds;
            }
            if let Some(mentions_me) = mentions_me {
                m.mentions_me = mentions_me;
            }
            changed = *m != before;
        });
        changed
    }

    fn names(&self) -> Names<'_> {
        Names { nicks: &self.nicks, roles: &self.role_names, channels: self }
    }

    fn preview(&self, d: &Value, mentions_me: bool) -> Option<DiscordMessage> {
        messages::preview(d, mentions_me, &self.names())
    }

    /// Takes the nick from a member object; `"nick": null` forgets it.
    fn remember_nick(&mut self, user: u64, member: &Value) {
        match member.get("nick") {
            Some(Value::String(nick)) if !nick.is_empty() => {
                if self.nicks.len() >= MAX_NICKS && !self.nicks.contains_key(&user) {
                    self.nicks.clear();
                }
                self.nicks.insert(user, nick.clone());
            }
            Some(_) => {
                self.nicks.remove(&user);
            }
            None => {}
        }
    }

    /// The channel's read-up-to message, falling back to the join time.
    fn acked(&self, channel_id: u64) -> Option<u64> {
        self.read_states.get(&channel_id).and_then(|s| s.last_acked).or(self.joined)
    }

    /// Every unread message of the channel is in its buffer (or it is read).
    fn complete(&self, channel_id: u64) -> bool {
        let Some(channel) = self.channels.get(&channel_id) else { return true };
        let known = self.buffers.get(&channel_id).and_then(|b| b.through).max(self.acked(channel_id));
        channel.last_message_id <= known
    }

    /// Listed unread channels whose unread messages are not all known, in
    /// display order; they are marked pending until [`Self::load_history`].
    pub fn take_fetches(&mut self, now: Timestamp) -> Vec<u64> {
        let unread: Vec<u64> = self.listed(now).iter().filter(|l| l.unread).map(|l| l.channel.id).collect();
        let wanted: Vec<u64> = unread.into_iter()
            .filter(|id| {
                !self.pending.contains(id)
                    && !self.complete(*id)
                    && self.failed.get(id).is_none_or(|at| now.duration_since(*at) >= FETCH_RETRY)
            })
            .collect();
        self.pending.extend(&wanted);
        wanted
    }

    /// Merges a channel's fetched history; returns whether anything changed.
    pub fn load_history(&mut self, channel_id: u64, result: Fetched, now: Timestamp) -> bool {
        self.pending.remove(&channel_id);
        let Some(latest) = self.channels.get(&channel_id).map(|c| c.last_message_id) else { return false };
        let list = match result {
            Fetched::Messages(list) => list,
            Fetched::Unavailable => {
                let buffer = self.buffers.entry(channel_id).or_default();
                buffer.through = buffer.through.max(latest);
                return true;
            }
            Fetched::Failed => {
                self.failed.insert(channel_id, now);
                return false;
            }
        };
        self.failed.remove(&channel_id);
        debug!("discord: loaded {} messages of channel {channel_id}", list.len());

        let acked = self.acked(channel_id).unwrap_or(0);
        let mut messages: Vec<(u64, &Value)> = list.iter().filter_map(|m| Some((snowflake(m.get("id"))?, m))).collect();
        messages.sort_by_key(|(id, _)| Reverse(*id));
        // A message of our own reads everything before it.
        let floor = messages.iter()
            .filter(|(_, m)| self.user_id.is_some() && snowflake(m.pointer("/author/id")) == self.user_id)
            .map(|(id, _)| *id)
            .fold(acked, u64::max);
        let newest = messages.first().map(|(id, _)| *id);
        if let Some(channel) = self.channels.get_mut(&channel_id) {
            channel.last_message_id = channel.last_message_id.max(newest);
        }
        // A full page that is unread throughout: older unread ones exist.
        let more = messages.len() >= history::PAGE && messages.last().is_some_and(|(oldest, _)| *oldest > floor);

        let buffered = |id: u64| self.buffers.get(&channel_id).is_some_and(|b| b.contains(id));
        let mut previews = heron_core::DISCORD_PREVIEW_MESSAGES;
        let entries: Vec<(u64, Option<DiscordMessage>)> = messages.iter()
            .filter(|(id, m)| *id > floor && !snowflake(m.pointer("/author/id")).is_some_and(|a| self.blocked.contains(&a)))
            .map(|(id, m)| {
                // Keep previews built from gateway events: they carry nicks.
                let preview = (previews > 0).then(|| {
                    previews -= 1;
                    if buffered(*id) { None } else { self.preview(m, self.mentions_me(m)) }
                });
                (*id, preview.flatten())
            })
            .collect();

        let buffer = self.buffers.entry(channel_id).or_default();
        for (id, preview) in entries {
            buffer.insert(id, preview);
        }
        if more {
            buffer.mark_more();
        }
        if floor > acked {
            buffer.ack(floor);
        }
        // Later messages arrive as events and are appended.
        buffer.through = buffer.through.max(latest.max(newest));
        true
    }

    fn mentions_me(&self, d: &Value) -> bool {
        let Some(me) = self.user_id else { return false };
        let users = d.get("mentions").and_then(Value::as_array).into_iter().flatten();
        if users.filter_map(|u| snowflake(u.get("id"))).any(|id| id == me) {
            return true;
        }
        if !self.settings.suppress_everyone && d.get("mention_everyone").and_then(Value::as_bool) == Some(true) {
            return true;
        }
        !self.settings.suppress_roles
            && snowflakes(d.get("mention_roles")).iter().any(|r| self.member_roles.contains(r))
    }

    /// With "mention on all messages", messages in unmuted channels set to
    /// all messages count towards the mention badge.
    fn low_importance_mention(&self, channel_id: u64, now: Timestamp) -> bool {
        if self.user_flags & USER_MENTION_ON_ALL_MESSAGES == 0 {
            return false;
        }
        let Some(channel) = self.channels.get(&channel_id) else { return false };
        !self.muted(channel, now) && self.notification_level(channel) == NOTIFY_ALL_MESSAGES
    }

    fn message_ack(&mut self, d: &Value) -> bool {
        let (Some(channel_id), Some(message_id)) = (snowflake(d.get("channel_id")), snowflake(d.get("message_id"))) else {
            return false;
        };
        if !self.channels.contains_key(&channel_id) {
            return false;
        }
        let manual = d.get("manual").and_then(Value::as_bool) == Some(true);
        let state = self.read_states.entry(channel_id).or_default();
        let previous = state.last_acked;
        if !manual && previous.is_some_and(|acked| acked > message_id) {
            return false;
        }
        state.last_acked = Some(message_id);
        state.mentions = count(d.get("mention_count"));
        if previous.is_some_and(|previous| previous > message_id) {
            // Marked unread: messages before the old read state were never
            // buffered, so fetch again.
            self.buffers.remove(&channel_id);
        } else if let Some(buffer) = self.buffers.get_mut(&channel_id) {
            buffer.ack(message_id);
        }
        true
    }

    /// What to acknowledge to mark `channel` read (or every listed channel):
    /// `(channel, newest message)` for each one that is unread or has
    /// mentions. `None` when the channel is unknown.
    pub fn ack_targets(&self, channel: Option<u64>, now: Timestamp) -> Option<Vec<(u64, u64)>> {
        let pending = |c: &Channel| {
            let mentions = self.read_states.get(&c.id).map_or(0, |s| s.mentions);
            let latest = c.last_message_id?;
            (Some(latest) > self.acked(c.id) || mentions > 0).then_some((c.id, latest))
        };
        match channel {
            Some(id) => Some(pending(self.channels.get(&id)?).into_iter().collect()),
            None => Some(self.listed(now).iter().filter_map(|l| pending(l.channel)).collect()),
        }
    }

    /// Applies acknowledgements Discord accepted, without waiting for the
    /// gateway to echo them.
    pub fn mark_read(&mut self, targets: &[(u64, u64)]) -> bool {
        let mut changed = false;
        for (channel, message) in targets {
            let ack = serde_json::json!({"channel_id": channel.to_string(), "message_id": message.to_string(), "mention_count": 0});
            changed |= self.message_ack(&ack);
        }
        changed
    }

    /// Guild, channel or parent category muted, honouring mute end times.
    fn muted(&self, channel: &Channel, now: Timestamp) -> bool {
        self.settings.mute.active(now)
            || self.ancestry(channel).any(|id| self.settings.overrides.get(&id).is_some_and(|o| o.mute.active(now)))
    }

    /// The channel and its category.
    fn ancestry(&self, channel: &Channel) -> impl Iterator<Item = u64> {
        std::iter::once(channel.id).chain(channel.parent)
    }

    /// Explicit "unreads: only mentions" on the channel, category or guild.
    fn unreads_only_mentions(&self, channel: &Channel) -> bool {
        for id in self.ancestry(channel) {
            let flags = self.settings.overrides.get(&id).map_or(0, |o| o.flags);
            if flags & CHANNEL_UNREADS_ALL_MESSAGES != 0 {
                return false;
            }
            if flags & CHANNEL_UNREADS_ONLY_MENTIONS != 0 {
                return true;
            }
        }
        self.settings.flags & GUILD_UNREADS_ALL_MESSAGES == 0 && self.settings.flags & GUILD_UNREADS_ONLY_MENTIONS != 0
    }

    /// False for channels hidden by the "show all channels" opt-in mode.
    fn opted_in(&self, channel: &Channel) -> bool {
        let flags = self.settings.flags;
        if flags & GUILD_OPT_IN_CHANNELS_ON == 0 || flags & GUILD_OPT_IN_CHANNELS_OFF != 0 {
            return true;
        }
        self.ancestry(channel)
            .any(|id| self.settings.overrides.get(&id).is_some_and(|o| o.flags & CHANNEL_OPT_IN_ENABLED != 0))
    }

    fn notification_level(&self, channel: &Channel) -> u64 {
        self.ancestry(channel)
            .filter_map(|id| self.settings.overrides.get(&id)?.message_notifications)
            .chain(self.settings.message_notifications)
            .chain(self.default_notifications)
            .find(|level| *level != NOTIFY_PARENT_DEFAULT)
            .unwrap_or(1)
    }

    /// Viewable text channels that are unread (and not muted) or have
    /// mentions, most mentions first, then in sidebar order.
    fn listed(&self, now: Timestamp) -> Vec<Listed<'_>> {
        let (Some(user_id), true) = (self.user_id, self.present) else { return Vec::new() };
        let member = Member {
            guild_id: self.guild_id,
            user_id,
            owner_id: self.owner_id,
            roles: &self.roles,
            member_roles: &self.member_roles,
        };
        let mut listed = Vec::new();
        for channel in self.channels.values().filter(|c| c.is_text()) {
            if !member.can_view(&channel.overwrites) {
                continue;
            }
            let mentions = self.read_states.get(&channel.id).map_or(0, |s| s.mentions);
            let unread = channel.last_message_id > self.acked(channel.id);
            let shown = unread && !self.muted(channel, now) && !self.unreads_only_mentions(channel) && self.opted_in(channel);
            if !shown && mentions == 0 {
                continue;
            }
            let category = channel.parent.and_then(|p| self.channels.get(&p)).map_or(-1, |c| c.position);
            let key = (Reverse(mentions), category, channel.position, channel.id);
            listed.push(Listed { channel, mentions, unread, shown, key });
        }
        listed.sort_by_key(|l| l.key);
        listed
    }

    /// The published view of [`Self::listed`] channels with unread counts
    /// and previews. The guild's message total counts unmuted channels only.
    pub fn snapshot(&self, now: Timestamp) -> Option<DiscordGuild> {
        self.user_id?;
        if !self.present {
            return None;
        }
        let mut unread_messages = 0u32;
        let channels: Vec<DiscordChannel> = self.listed(now).into_iter().map(|l| {
            let id = l.channel.id;
            let buffer = self.buffers.get(&id).filter(|_| l.unread);
            let (mut unread, more) = buffer.map_or((0, false), Buffer::count);
            if l.unread && unread == 0 && !self.complete(id) {
                // History not loaded yet; it is at least one.
                unread = 1;
            }
            let unread = unread.min(DISCORD_UNREAD_CAP);
            if l.shown {
                unread_messages = unread_messages.saturating_add(unread);
            }
            DiscordChannel {
                id: id.to_string(),
                name: l.channel.name.clone(),
                unread,
                more,
                mentions: l.mentions,
                url: format!("https://discord.com/channels/{}/{id}", self.guild_id),
                messages: buffer.map(Buffer::previews).unwrap_or_default(),
            }
        }).collect();

        Some(DiscordGuild {
            id: self.guild_id.to_string(),
            name: self.name.clone(),
            unread_channels: channels.len() as u32,
            unread_messages,
            mentions: channels.iter().map(|c| c.mentions).fold(0, u32::saturating_add),
            channels,
        })
    }
}

impl messages::ChannelNames for GuildState {
    fn channel_name(&self, id: u64) -> Option<String> {
        self.channels.get(&id).map(|c| c.name.clone())
    }
}

/// Top-level field, or one inside `properties` as in user-account READY.
fn guild_field<'a>(guild: &'a Value, key: &str) -> Option<&'a Value> {
    guild.get(key).or_else(|| guild.get("properties")?.get(key))
}

/// Snowflakes arrive as strings; numbers are accepted too. Zero is absent.
fn snowflake(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    value.as_str().and_then(|s| s.parse().ok()).or_else(|| value.as_u64()).filter(|id| *id != 0)
}

fn snowflakes(value: Option<&Value>) -> Vec<u64> {
    value.and_then(Value::as_array).into_iter().flatten().filter_map(|v| snowflake(Some(v))).collect()
}

/// Permission bitfields are decimal strings.
fn bits(value: Option<&Value>) -> u64 {
    value.and_then(|v| v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64())).unwrap_or(0)
}

fn count(value: Option<&Value>) -> u32 {
    value.and_then(Value::as_u64).map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// The smallest snowflake created at `time`, for read-up-to comparisons.
fn timestamp_snowflake(time: &str) -> Option<u64> {
    let ms = time.parse::<Timestamp>().ok()?.as_millisecond() - DISCORD_EPOCH_MS;
    u64::try_from(ms).ok().map(|ms| ms << 22)
}

fn parse_role(value: &Value) -> Option<(u64, u64)> {
    Some((snowflake(value.get("id"))?, bits(value.get("permissions"))))
}

fn parse_channel(value: &Value) -> Option<Channel> {
    let overwrites = value.get("permission_overwrites").and_then(Value::as_array).into_iter().flatten()
        .filter_map(|o| {
            let kind = match o.get("type").and_then(|t| t.as_u64().or_else(|| t.as_str()?.parse().ok()))? {
                0 => OverwriteKind::Role,
                1 => OverwriteKind::Member,
                _ => return None,
            };
            Some(Overwrite { id: snowflake(o.get("id"))?, kind, allow: bits(o.get("allow")), deny: bits(o.get("deny")) })
        })
        .collect();
    let id = snowflake(value.get("id"))?;
    Some(Channel {
        id,
        kind: value.get("type").and_then(Value::as_u64)?,
        name: value.get("name").and_then(Value::as_str).map_or_else(|| id.to_string(), str::to_owned),
        parent: snowflake(value.get("parent_id")),
        position: value.get("position").and_then(Value::as_i64).unwrap_or(0),
        last_message_id: snowflake(value.get("last_message_id")),
        overwrites,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const READY: &str = include_str!("fixtures/ready.json");
    const R: u64 = 1_200_000_000_000_000_000;

    fn now() -> Timestamp {
        "2026-10-22T12:00:00Z".parse().unwrap()
    }

    fn ready() -> GuildState {
        let mut state = GuildState::new(1000);
        assert!(state.apply("READY", &serde_json::from_str(READY).unwrap()));
        state
    }

    fn names(state: &GuildState) -> Vec<(String, u32)> {
        state.snapshot(now()).unwrap().channels.into_iter().map(|c| (c.name, c.mentions)).collect()
    }

    fn pairs(list: &[(&str, u32)]) -> Vec<(String, u32)> {
        list.iter().map(|(n, m)| (n.to_string(), *m)).collect()
    }

    fn message(channel: u64, id: u64, author: u64, extra: Value) -> Value {
        let mut m = json!({
            "id": id.to_string(), "channel_id": channel.to_string(), "guild_id": "1000",
            "author": { "id": author.to_string() }, "type": 0, "content": "",
            "mentions": [], "mention_roles": [], "mention_everyone": false,
        });
        m.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        m
    }

    #[test]
    fn ready_parsing() {
        let state = ready();
        assert!(state.present());
        assert_eq!(state.user_id, Some(7));
        assert_eq!(state.name, "My Server");
        assert_eq!(state.owner_id, Some(9));
        assert_eq!(state.member_roles, [2000]);
        assert_eq!(state.roles.get(&1000), Some(&1024));
        assert_eq!(state.channels.len(), 14);
        assert_eq!(state.read_states.get(&3003), Some(&ReadState { last_acked: Some(R), mentions: 2 }));
        // Feature read states (type 1) are not channels.
        assert!(!state.read_states.contains_key(&555));
        assert!(state.settings.suppress_everyone);
        assert!(state.settings.overrides[&3010].mute.until.is_some());
        assert!(state.blocked.contains(&66) && !state.blocked.contains(&8));
        assert_eq!(state.joined, timestamp_snowflake("2020-01-01T00:00:00Z"));
    }

    #[test]
    fn unread_and_mentions() {
        let state = ready();
        let guild = state.snapshot(now()).unwrap();
        // mods: 2 mentions; announcements: muted but mentioned; then sidebar
        // order: uncategorised first, then by category. secret is hidden,
        // muted-chan and the muted category are excluded, the expired mute
        // is not, voice is ignored, old-no-readstate predates joining.
        assert_eq!(
            names(&state),
            pairs(&[("mods", 2), ("announcements", 1), ("new-no-readstate", 0), ("general", 0), ("expired-mute", 0)])
        );
        assert_eq!(guild.unread_channels, 5);
        assert_eq!(guild.mentions, 3);
        assert_eq!(guild.channels[0].url, "https://discord.com/channels/1000/3003");
        assert_eq!(guild.id, "1000");
    }

    #[test]
    fn mute_expiry() {
        let state = ready();
        let later: Timestamp = "3000-01-01T00:00:00Z".parse().unwrap();
        let listed: Vec<String> = state.snapshot(later).unwrap().channels.into_iter().map(|c| c.name).collect();
        assert!(listed.contains(&"in-muted-cat".to_owned()));
        assert!(!listed.contains(&"muted-chan".to_owned()));
    }

    #[test]
    fn guild_mute_hides_unreads_keeps_mentions() {
        let mut state = ready();
        let mut settings: Value = serde_json::from_str::<Value>(READY).unwrap()["user_guild_settings"]["entries"][1].clone();
        settings["muted"] = json!(true);
        assert!(state.apply("USER_GUILD_SETTINGS_UPDATE", &settings));
        assert_eq!(names(&state), pairs(&[("mods", 2), ("announcements", 1)]));
    }

    #[test]
    fn message_create_marks_unread_and_counts_mentions() {
        let mut state = ready();
        let other = 8;
        assert!(state.apply("MESSAGE_CREATE", &message(3002, R + 20, other, json!({}))));
        assert!(names(&state).contains(&("rules".into(), 0)));

        // Role mention counts; @everyone is suppressed in the fixture settings.
        state.apply("MESSAGE_CREATE", &message(3002, R + 21, other, json!({"mention_roles": ["2000"]})));
        state.apply("MESSAGE_CREATE", &message(3002, R + 22, other, json!({"mention_everyone": true})));
        state.apply("MESSAGE_CREATE", &message(3002, R + 23, other, json!({"mentions": [{"id": "7"}]})));
        assert!(names(&state).contains(&("rules".into(), 2)));

        // Mentions in a muted channel still count.
        state.apply("MESSAGE_CREATE", &message(3005, R + 24, other, json!({"mentions": [{"id": "7"}]})));
        assert!(names(&state).contains(&("muted-chan".into(), 1)));

        // Blocked users never mention.
        state.apply("MESSAGE_CREATE", &message(3001, R + 25, 66, json!({"mentions": [{"id": "7"}]})));
        assert!(names(&state).contains(&("general".into(), 0)));

        // Other guilds and unknown channels are ignored.
        assert!(!state.apply("MESSAGE_CREATE", &message(9999, R + 26, other, json!({}))));
        assert!(!state.apply("MESSAGE_CREATE", &message(3002, R + 27, other, json!({"guild_id": "555"}))));
    }

    #[test]
    fn own_message_acknowledges() {
        let mut state = ready();
        state.apply("MESSAGE_CREATE", &message(3003, R + 30, 7, json!({})));
        assert!(!names(&state).iter().any(|(n, _)| n == "mods"));
    }

    #[test]
    fn message_ack_clears() {
        let mut state = ready();
        let ack = json!({"channel_id": "3003", "message_id": (R + 5).to_string(), "mention_count": 0, "version": 5});
        assert!(state.apply("MESSAGE_ACK", &ack));
        assert!(!names(&state).iter().any(|(n, _)| n == "mods"));
        // An older ack is ignored.
        let stale = json!({"channel_id": "3003", "message_id": R.to_string(), "mention_count": 4, "version": 6});
        assert!(!state.apply("MESSAGE_ACK", &stale));
        // Partial ack leaves remaining mentions.
        let partial = json!({"channel_id": "3001", "message_id": R.to_string(), "mention_count": 0});
        state.apply("MESSAGE_ACK", &partial);
        assert!(names(&state).contains(&("general".into(), 0)));
    }

    #[test]
    fn mark_read_targets_and_applies() {
        let mut state = ready();
        let now = now();
        let all = state.ack_targets(None, now).unwrap();
        assert!(!all.is_empty());
        assert!(state.ack_targets(Some(999_999), now).is_none());
        let (channel, _) = all[0];
        assert_eq!(state.ack_targets(Some(channel), now).unwrap(), vec![all[0]]);
        assert!(state.mark_read(&all));
        assert_eq!(state.ack_targets(None, now).unwrap(), vec![]);
        assert_eq!(state.snapshot(now).unwrap().unread_channels, 0);
        assert_eq!(state.ack_targets(Some(channel), now).unwrap(), vec![]);
    }

    #[test]
    fn channel_unread_update_and_channel_events() {
        let mut state = ready();
        let update = json!({"guild_id": "1000", "channel_unread_updates": [
            {"id": "3002", "last_message_id": (R + 40).to_string()},
            {"id": "3040", "last_message_id": null}
        ]});
        assert!(state.apply("CHANNEL_UNREAD_UPDATE", &update));
        assert!(names(&state).contains(&("rules".into(), 0)));

        // Hiding a channel by overwrite removes it.
        let hide = json!({"id": "3002", "guild_id": "1000", "type": 0, "name": "rules", "parent_id": "3000", "position": 1,
            "permission_overwrites": [{"id": "1000", "type": 0, "allow": "0", "deny": "1024"}]});
        assert!(state.apply("CHANNEL_UPDATE", &hide));
        assert!(!names(&state).iter().any(|(n, _)| n == "rules"));
        // The update did not lose the newer last_message_id.
        assert_eq!(state.channels[&3002].last_message_id, Some(R + 40));

        assert!(state.apply("CHANNEL_DELETE", &json!({"id": "3001", "guild_id": "1000", "type": 0})));
        assert!(!names(&state).iter().any(|(n, _)| n == "general"));

        let created = json!({"id": "3050", "guild_id": "1000", "type": 0, "name": "fresh", "position": 9});
        assert!(state.apply("CHANNEL_CREATE", &created));
        state.apply("MESSAGE_CREATE", &message(3050, R + 50, 8, json!({})));
        assert!(names(&state).contains(&("fresh".into(), 0)));
    }

    #[test]
    fn member_and_role_updates_change_visibility() {
        let mut state = ready();
        let update = json!({"guild_id": "1000", "user": {"id": "7"}, "roles": []});
        assert!(state.apply("GUILD_MEMBER_UPDATE", &update));
        assert!(!names(&state).iter().any(|(n, _)| n == "mods"));

        // Someone else's update is ignored.
        let other = json!({"guild_id": "1000", "user": {"id": "8"}, "roles": ["2000"]});
        assert!(!state.apply("GUILD_MEMBER_UPDATE", &other));

        // Admin on @everyone reveals everything, including secret.
        let role = json!({"guild_id": "1000", "role": {"id": "1000", "name": "@everyone", "permissions": "8"}});
        assert!(state.apply("GUILD_ROLE_UPDATE", &role));
        assert!(names(&state).iter().any(|(n, _)| n == "secret"));
    }

    #[test]
    fn unreads_only_mentions_flag() {
        let mut state = ready();
        let mut settings: Value = serde_json::from_str::<Value>(READY).unwrap()["user_guild_settings"]["entries"][1].clone();
        settings["flags"] = json!(GUILD_UNREADS_ONLY_MENTIONS);
        settings["channel_overrides"].as_array_mut().unwrap()
            .push(json!({"channel_id": "3001", "muted": false, "flags": CHANNEL_UNREADS_ALL_MESSAGES}));
        state.apply("USER_GUILD_SETTINGS_UPDATE", &settings);
        assert_eq!(names(&state), pairs(&[("mods", 2), ("announcements", 1), ("general", 0)]));
    }

    #[test]
    fn mention_on_all_messages() {
        let mut state = ready();
        state.user_flags = USER_MENTION_ON_ALL_MESSAGES;
        // in-muted-cat is set to all messages but its category is muted.
        state.apply("MESSAGE_CREATE", &message(3011, R + 60, 8, json!({})));
        assert_eq!(state.read_states[&3011].mentions, 0);
        // general follows the guild (only mentions).
        state.apply("MESSAGE_CREATE", &message(3001, R + 61, 8, json!({})));
        assert_eq!(state.read_states[&3001].mentions, 0);
        state.settings.message_notifications = Some(NOTIFY_ALL_MESSAGES);
        state.apply("MESSAGE_CREATE", &message(3001, R + 62, 8, json!({})));
        assert_eq!(state.read_states[&3001].mentions, 1);
    }

    #[test]
    fn missing_or_deleted_guild() {
        let mut state = GuildState::new(4242);
        state.apply("READY", &serde_json::from_str(READY).unwrap());
        assert!(!state.present());
        assert_eq!(state.snapshot(now()), None);

        let mut state = ready();
        assert!(state.apply("GUILD_DELETE", &json!({"id": "1000", "unavailable": true})));
        assert_eq!(state.snapshot(now()), None);
        let guild: Value = serde_json::from_str::<Value>(READY).unwrap()["guilds"][1].clone();
        assert!(state.apply("GUILD_CREATE", &guild));
        assert_eq!(state.snapshot(now()).unwrap().unread_channels, 5);
    }

    const HISTORY_GENERAL: &str = include_str!("fixtures/history_general.json");

    fn channel(state: &GuildState, name: &str) -> DiscordChannel {
        state.snapshot(now()).unwrap().channels.into_iter().find(|c| c.name == name).unwrap()
    }

    fn contents(c: &DiscordChannel) -> Vec<&str> {
        c.messages.iter().map(|m| m.content.as_str()).collect()
    }

    /// REST shape, newest first: `ids` by user 8 with content = the offset.
    fn page(channel: u64, offsets: impl Iterator<Item = u64>) -> Fetched {
        let mut list: Vec<Value> = offsets.map(|o| {
            let mut m = message(channel, R + o, 8, json!({"content": format!("m{o}")}));
            m.as_object_mut().unwrap().remove("guild_id");
            m
        }).collect();
        list.sort_by_key(|m| Reverse(snowflake(m.get("id"))));
        Fetched::Messages(list)
    }

    fn loaded() -> GuildState {
        let mut state = ready();
        state.take_fetches(now());
        state.load_history(3001, Fetched::Messages(serde_json::from_str(HISTORY_GENERAL).unwrap()), now());
        state
    }

    #[test]
    fn fetches_unread_channels_once_in_display_order() {
        let mut state = ready();
        // Before history arrives each unread channel counts as one.
        let general = channel(&state, "general");
        assert_eq!((general.unread, general.more, general.messages.len()), (1, false, 0));
        // announcements is muted (listed for its mention) but unread too.
        assert_eq!(state.take_fetches(now()), [3003, 3006, 3041, 3001, 3021]);
        assert!(state.take_fetches(now()).is_empty());
        // A failure is retried, but not right away.
        assert!(!state.load_history(3003, Fetched::Failed, now()));
        assert!(state.take_fetches(now()).is_empty());
        assert_eq!(state.take_fetches(now() + SignedDuration::from_mins(6)), [3003]);
        // An unreadable channel is not asked for again.
        assert!(state.load_history(3021, Fetched::Unavailable, now()));
        assert!(state.take_fetches(now() + SignedDuration::from_mins(60)).iter().all(|id| *id != 3021));
    }

    #[test]
    fn history_counts_and_previews() {
        let state = loaded();
        let general = channel(&state, "general");
        assert_eq!((general.unread, general.more), (1, false));
        assert_eq!(general.messages.len(), 1);
        let m = &general.messages[0];
        assert_eq!(m.id, (R + 1).to_string());
        assert_eq!(m.author, "Friend");
        assert_eq!(m.content, "@mod look at #mods");
        assert_eq!(m.timestamp, 1_792_670_340);
        // Role mention of a role the user has.
        assert!(m.mentions_me);
    }

    #[test]
    fn cap_and_more() {
        let mut state = ready();
        state.take_fetches(now());
        // mods is read up to R; a full page of unread messages R+1..=R+100.
        assert!(state.load_history(3003, page(3003, 1..=100), now()));
        let mods = channel(&state, "mods");
        assert_eq!((mods.unread, mods.more), (100, true));
        assert_eq!(contents(&mods), ["m98", "m99", "m100"]);

        // A full page that reaches back to read messages is exact.
        state.load_history(3041, page(3041, 0..100), now());
        let joined = state.joined.unwrap();
        assert!(R > joined);
        let new = channel(&state, "new-no-readstate");
        assert_eq!((new.unread, new.more), (100, true));
        state.load_history(3021, page(3021, 0..100), now());
        let expired = channel(&state, "expired-mute");
        assert_eq!((expired.unread, expired.more), (99, false));

        // New messages keep the count capped.
        state.apply("MESSAGE_CREATE", &message(3003, R + 101, 8, json!({"content": "m101"})));
        let mods = channel(&state, "mods");
        assert_eq!((mods.unread, mods.more), (100, true));
        assert_eq!(contents(&mods), ["m99", "m100", "m101"]);

        let guild = state.snapshot(now()).unwrap();
        // mods 100 + new 100 + general (not loaded: 1) + expired 99; the
        // muted announcements channel is listed but not totalled.
        assert_eq!(guild.unread_messages, 300);
        assert!(guild.channels.iter().all(|c| c.messages.len() <= heron_core::DISCORD_PREVIEW_MESSAGES));
    }

    #[test]
    fn live_messages_append_after_history() {
        let mut state = loaded();
        state.apply("MESSAGE_CREATE", &message(3001, R + 2, 8, json!({"content": "two", "member": {"nick": "Pal"}})));
        state.apply("MESSAGE_CREATE", &message(3001, R + 3, 8, json!({"content": "three"})));
        state.apply("MESSAGE_CREATE", &message(3001, R + 4, 66, json!({"content": "blocked"})));
        let general = channel(&state, "general");
        assert_eq!(general.unread, 3);
        assert_eq!(contents(&general), ["@mod look at #mods", "two", "three"]);
        // The nick learned from the event is used for later messages.
        assert_eq!(general.messages[2].author, "Pal");
        // Blocked messages leave the buffer complete: no fetch.
        assert!(state.take_fetches(now()).iter().all(|id| *id != 3001));
    }

    #[test]
    fn read_channel_becomes_unread_without_fetch() {
        let mut state = ready();
        state.apply("MESSAGE_CREATE", &message(3002, R + 20, 8, json!({"content": "<:wave:12> hi"})));
        let rules = channel(&state, "rules");
        assert_eq!((rules.unread, contents(&rules)), (1, vec![":wave: hi"]));
        assert!(!state.take_fetches(now()).contains(&3002));
    }

    #[test]
    fn reply_ping_detection() {
        let mut state = ready();
        let reply = |id: u64, pings: bool| {
            message(3002, R + id, 8, json!({
                "type": 19, "content": "reply",
                "mentions": if pings { json!([{"id": "7", "username": "owner"}]) } else { json!([]) },
                "message_reference": {"message_id": "5"},
                "referenced_message": {"id": "5", "author": {"id": "7", "username": "owner"}},
            }))
        };
        state.apply("MESSAGE_CREATE", &reply(1, true));
        state.apply("MESSAGE_CREATE", &reply(2, false));
        // @everyone is suppressed in the fixture settings.
        state.apply("MESSAGE_CREATE", &message(3002, R + 3, 8, json!({"mention_everyone": true})));
        let rules = channel(&state, "rules");
        let flags: Vec<bool> = rules.messages.iter().map(|m| m.mentions_me).collect();
        assert_eq!(flags, [true, false, false]);
        assert_eq!(rules.mentions, 1);
    }

    #[test]
    fn ack_trims_and_own_message_clears() {
        let mut state = ready();
        state.take_fetches(now());
        state.load_history(3003, page(3003, 1..=10), now());
        let ack = json!({"channel_id": "3003", "message_id": (R + 7).to_string(), "mention_count": 1});
        assert!(state.apply("MESSAGE_ACK", &ack));
        let mods = channel(&state, "mods");
        assert_eq!((mods.unread, mods.mentions), (3, 1));
        assert_eq!(contents(&mods), ["m8", "m9", "m10"]);
        let ack = json!({"channel_id": "3003", "message_id": (R + 9).to_string(), "mention_count": 1});
        state.apply("MESSAGE_ACK", &ack);
        assert_eq!(contents(&channel(&state, "mods")), ["m10"]);

        state.apply("MESSAGE_CREATE", &message(3003, R + 11, 7, json!({})));
        assert!(!names(&state).iter().any(|(n, _)| n == "mods"));
        assert_eq!(state.buffers[&3003].count(), (0, false));
    }

    #[test]
    fn ack_clears_more_once_past_the_buffer() {
        let mut state = ready();
        state.take_fetches(now());
        state.load_history(3003, page(3003, 1..=100), now());
        let ack = json!({"channel_id": "3003", "message_id": (R + 50).to_string(), "mention_count": 0});
        state.apply("MESSAGE_ACK", &ack);
        let mods = channel(&state, "mods");
        assert_eq!((mods.unread, mods.more), (50, false));
    }

    #[test]
    fn manual_mark_unread_refetches() {
        let mut state = loaded();
        let ack = json!({"channel_id": "3001", "message_id": (R - 5).to_string(), "mention_count": 0, "manual": true});
        assert!(state.apply("MESSAGE_ACK", &ack));
        assert!(state.take_fetches(now()).contains(&3001));
    }

    #[test]
    fn own_message_in_history_reads_older_ones() {
        let mut state = ready();
        state.take_fetches(now());
        let Fetched::Messages(mut list) = page(3003, 1..=5) else { unreachable!() };
        list[2]["author"]["id"] = json!("7"); // R+3
        state.load_history(3003, Fetched::Messages(list), now());
        assert_eq!(contents(&channel(&state, "mods")), ["m4", "m5"]);
    }

    #[test]
    fn deletes_and_edits() {
        let mut state = ready();
        state.take_fetches(now());
        state.load_history(3003, page(3003, 1..=6), now());
        let delete = json!({"id": (R + 6).to_string(), "channel_id": "3003", "guild_id": "1000"});
        assert!(state.apply("MESSAGE_DELETE", &delete));
        assert!(!state.apply("MESSAGE_DELETE", &delete));
        let bulk = json!({"ids": [(R + 1).to_string(), (R + 2).to_string(), "1"], "channel_id": "3003", "guild_id": "1000"});
        assert!(state.apply("MESSAGE_DELETE_BULK", &bulk));
        let mods = channel(&state, "mods");
        assert_eq!(mods.unread, 3);
        assert_eq!(contents(&mods), ["m4", "m5"]);

        let mut edit = message(3003, R + 5, 8, json!({"content": "edited <@7>", "mentions": [{"id": "7", "username": "owner", "global_name": "Owner"}]}));
        edit["edited_timestamp"] = json!("2026-10-22T12:01:00Z");
        assert!(state.apply("MESSAGE_UPDATE", &edit));
        let mods = channel(&state, "mods");
        assert_eq!(contents(&mods), ["m4", "edited @Owner"]);
        assert!(mods.messages[1].mentions_me);
        // An embed-only update without content changes nothing.
        let unfurl = json!({"id": (R + 5).to_string(), "channel_id": "3003", "guild_id": "1000", "embeds": [{}]});
        assert!(!state.apply("MESSAGE_UPDATE", &unfurl));
        // A bot's embed arriving later shows up.
        let rich = json!({"id": (R + 5).to_string(), "channel_id": "3003", "guild_id": "1000", "embeds": [{"title": "Alert"}]});
        assert!(state.apply("MESSAGE_UPDATE", &rich));
        assert_eq!(channel(&state, "mods").messages[1].embeds[0].title.as_deref(), Some("Alert"));
        assert_eq!(contents(&channel(&state, "mods")), ["m4", "edited @Owner"]);
    }

    #[test]
    fn missed_messages_trigger_a_fetch() {
        let mut state = loaded();
        assert!(state.take_fetches(now()).iter().all(|id| *id != 3001));
        // Discord reports a newer message we never saw (reconnect gap).
        let update = json!({"guild_id": "1000", "channel_unread_updates": [{"id": "3001", "last_message_id": (R + 9).to_string()}]});
        state.apply("CHANNEL_UNREAD_UPDATE", &update);
        assert!(state.take_fetches(now()).contains(&3001));
        // The refetch merges with what was buffered.
        state.load_history(3001, page(3001, 1..=9), now());
        let general = channel(&state, "general");
        assert_eq!(general.unread, 9);
        // Previews are the newest three of the merged buffer.
        assert_eq!(contents(&general), ["m7", "m8", "m9"]);
    }
}
