//! Channel permissions for the current member, enough to decide `VIEW_CHANNEL`.
//!
//! Adapted from concord (github.com/chojs23/concord, GPL-3.0),
//! `src/discord/permission.rs`.

use std::collections::HashMap;

pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const ALL: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverwriteKind {
    Role,
    Member,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overwrite {
    pub id: u64,
    pub kind: OverwriteKind,
    pub allow: u64,
    pub deny: u64,
}

/// The member's identity within a guild.
pub struct Member<'a> {
    pub guild_id: u64,
    pub user_id: u64,
    pub owner_id: Option<u64>,
    /// Role id to permission bits, including `@everyone` (id = guild id).
    pub roles: &'a HashMap<u64, u64>,
    pub member_roles: &'a [u64],
}

impl Member<'_> {
    /// Guild-level permissions: `@everyone` plus every role the member has.
    pub fn base(&self) -> u64 {
        if self.owner_id == Some(self.user_id) {
            return ALL;
        }
        let mut perms = self.roles.get(&self.guild_id).copied().unwrap_or(0);
        for role in self.member_roles {
            perms |= self.roles.get(role).copied().unwrap_or(0);
        }
        if perms & ADMINISTRATOR != 0 { ALL } else { perms }
    }

    /// Applies overwrites in Discord's order: `@everyone`, then the union of
    /// the member's roles, then the member itself.
    pub fn in_channel(&self, overwrites: &[Overwrite]) -> u64 {
        let base = self.base();
        if base == ALL {
            return ALL;
        }
        let mut perms = base;
        if let Some(o) = overwrites.iter().find(|o| o.kind == OverwriteKind::Role && o.id == self.guild_id) {
            perms = (perms & !o.deny) | o.allow;
        }
        let (mut allow, mut deny) = (0, 0);
        for o in overwrites {
            if o.kind == OverwriteKind::Role && o.id != self.guild_id && self.member_roles.contains(&o.id) {
                allow |= o.allow;
                deny |= o.deny;
            }
        }
        perms = (perms & !deny) | allow;
        if let Some(o) = overwrites.iter().find(|o| o.kind == OverwriteKind::Member && o.id == self.user_id) {
            perms = (perms & !o.deny) | o.allow;
        }
        perms
    }

    pub fn can_view(&self, overwrites: &[Overwrite]) -> bool {
        self.in_channel(overwrites) & VIEW_CHANNEL != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUILD: u64 = 100;
    const ME: u64 = 7;
    const MOD: u64 = 200;
    const ADMIN: u64 = 300;

    fn roles() -> HashMap<u64, u64> {
        HashMap::from([(GUILD, VIEW_CHANNEL), (MOD, 0), (ADMIN, ADMINISTRATOR)])
    }

    fn ow(id: u64, kind: OverwriteKind, allow: u64, deny: u64) -> Overwrite {
        Overwrite { id, kind, allow, deny }
    }

    #[test]
    fn everyone_deny_and_role_allow() {
        let roles = roles();
        let hidden = [ow(GUILD, OverwriteKind::Role, 0, VIEW_CHANNEL)];
        let mod_only = [ow(GUILD, OverwriteKind::Role, 0, VIEW_CHANNEL), ow(MOD, OverwriteKind::Role, VIEW_CHANNEL, 0)];

        let plain = Member { guild_id: GUILD, user_id: ME, owner_id: None, roles: &roles, member_roles: &[] };
        assert!(plain.can_view(&[]));
        assert!(!plain.can_view(&hidden));
        assert!(!plain.can_view(&mod_only));

        let moderator = Member { member_roles: &[MOD], ..plain };
        assert!(moderator.can_view(&mod_only));
    }

    #[test]
    fn member_overwrite_wins_over_roles() {
        let roles = roles();
        let overwrites = [ow(MOD, OverwriteKind::Role, VIEW_CHANNEL, 0), ow(ME, OverwriteKind::Member, 0, VIEW_CHANNEL)];
        let member = Member { guild_id: GUILD, user_id: ME, owner_id: None, roles: &roles, member_roles: &[MOD] };
        assert!(!member.can_view(&overwrites));
    }

    #[test]
    fn role_allow_beats_role_deny() {
        let roles = roles();
        let other = 400;
        let overwrites = [ow(MOD, OverwriteKind::Role, VIEW_CHANNEL, 0), ow(other, OverwriteKind::Role, 0, VIEW_CHANNEL)];
        let member = Member { guild_id: GUILD, user_id: ME, owner_id: None, roles: &roles, member_roles: &[MOD, other] };
        assert!(member.can_view(&overwrites));
    }

    #[test]
    fn admin_and_owner_bypass() {
        let roles = roles();
        let hidden = [ow(GUILD, OverwriteKind::Role, 0, VIEW_CHANNEL), ow(ME, OverwriteKind::Member, 0, VIEW_CHANNEL)];
        let admin = Member { guild_id: GUILD, user_id: ME, owner_id: None, roles: &roles, member_roles: &[ADMIN] };
        assert!(admin.can_view(&hidden));
        let owner = Member { owner_id: Some(ME), member_roles: &[], ..admin };
        assert!(owner.can_view(&hidden));
    }
}
