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

use heron_core::{DiscordChannel, DiscordGuild};
use jiff::Timestamp;
use serde_json::Value;

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
                d.get("role").and_then(parse_role).map(|(id, perms)| self.roles.insert(id, perms)).is_some()
            }
            "GUILD_ROLE_DELETE" if self.is_ours(d.get("guild_id")) => {
                snowflake(d.get("role_id")).and_then(|id| self.roles.remove(&id)).is_some()
            }
            "GUILD_MEMBER_UPDATE" if self.is_ours(d.get("guild_id")) => {
                if snowflake(d.pointer("/user/id")).is_none_or(|id| Some(id) != self.user_id) {
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
        self.roles = g.get("roles").and_then(Value::as_array).into_iter().flatten().filter_map(parse_role).collect();
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
        let Some(channel) = self.channels.get_mut(&channel_id) else { return false };
        channel.last_message_id = channel.last_message_id.max(Some(message_id));

        let author = snowflake(d.pointer("/author/id"));
        if author.is_some() && author == self.user_id {
            // Sending a message acknowledges the channel.
            if d.get("type").and_then(Value::as_u64) != Some(MESSAGE_POLL_RESULT) {
                let state = self.read_states.entry(channel_id).or_default();
                state.last_acked = state.last_acked.max(Some(message_id));
                state.mentions = 0;
            }
            return true;
        }
        if author.is_some_and(|a| self.blocked.contains(&a)) {
            return true;
        }
        let acked = self.read_states.get(&channel_id).and_then(|s| s.last_acked).or(self.joined);
        if acked.is_some_and(|acked| acked >= message_id) {
            return true;
        }
        if self.mentions_me(d) || self.low_importance_mention(channel_id, Timestamp::now()) {
            let state = self.read_states.entry(channel_id).or_default();
            state.mentions = state.mentions.saturating_add(1);
        }
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
        if !manual && state.last_acked.is_some_and(|acked| acked > message_id) {
            return false;
        }
        state.last_acked = Some(message_id);
        state.mentions = count(d.get("mention_count"));
        true
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

    /// The published view: viewable text channels that are unread (and not
    /// muted) or have mentions, most mentions first, then in sidebar order.
    pub fn snapshot(&self, now: Timestamp) -> Option<DiscordGuild> {
        let user_id = self.user_id?;
        if !self.present {
            return None;
        }
        let member = Member {
            guild_id: self.guild_id,
            user_id,
            owner_id: self.owner_id,
            roles: &self.roles,
            member_roles: &self.member_roles,
        };

        let mut listed: Vec<(_, DiscordChannel)> = Vec::new();
        for channel in self.channels.values().filter(|c| c.is_text()) {
            if !member.can_view(&channel.overwrites) {
                continue;
            }
            let state = self.read_states.get(&channel.id);
            let mentions = state.map_or(0, |s| s.mentions);
            let acked = state.and_then(|s| s.last_acked).or(self.joined);
            let unread = match (channel.last_message_id, acked) {
                (Some(latest), Some(acked)) => latest > acked,
                (Some(_), None) => true,
                (None, _) => false,
            };
            let shown = unread && !self.muted(channel, now) && !self.unreads_only_mentions(channel) && self.opted_in(channel);
            if !shown && mentions == 0 {
                continue;
            }
            let category = channel.parent.and_then(|p| self.channels.get(&p)).map_or(-1, |c| c.position);
            let key = (Reverse(mentions), category, channel.position, channel.id);
            listed.push((key, DiscordChannel {
                id: channel.id.to_string(),
                name: channel.name.clone(),
                mentions,
                url: format!("https://discord.com/channels/{}/{}", self.guild_id, channel.id),
                unread: 0,
                more: false,
                messages: Vec::new(),
            }));
        }
        listed.sort_by_key(|(key, _)| *key);
        let channels: Vec<DiscordChannel> = listed.into_iter().map(|(_, c)| c).collect();

        Some(DiscordGuild {
            id: self.guild_id.to_string(),
            name: self.name.clone(),
            unread_channels: channels.len() as u32,
            unread_messages: 0,
            mentions: channels.iter().map(|c| c.mentions).fold(0, u32::saturating_add),
            channels,
        })
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
}
