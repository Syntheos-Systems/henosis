/// Bitfield permissions — Discord-style
#[allow(dead_code)]
pub mod perms {
    pub const VIEW_CHANNELS: i64 = 1 << 0;
    pub const SEND_MESSAGES: i64 = 1 << 1;
    pub const READ_HISTORY: i64 = 1 << 2;
    pub const MANAGE_MESSAGES: i64 = 1 << 3;
    pub const ATTACH_FILES: i64 = 1 << 4;
    pub const MANAGE_CHANNELS: i64 = 1 << 5;
    pub const MANAGE_SERVER: i64 = 1 << 6;
    pub const MANAGE_ROLES: i64 = 1 << 7;
    pub const KICK_MEMBERS: i64 = 1 << 8;
    pub const BAN_MEMBERS: i64 = 1 << 9;
    pub const CREATE_INVITES: i64 = 1 << 10;
    pub const MANAGE_INVITES: i64 = 1 << 11;
    pub const MENTION_EVERYONE: i64 = 1 << 12;
    pub const ADMINISTRATOR: i64 = 1 << 30;

    /// Default permissions for @everyone role
    pub const DEFAULT: i64 =
        VIEW_CHANNELS | SEND_MESSAGES | READ_HISTORY | ATTACH_FILES | CREATE_INVITES;

    /// Check if a permission set includes a specific permission
    pub fn has(permissions: i64, permission: i64) -> bool {
        (permissions & ADMINISTRATOR) != 0 || (permissions & permission) != 0
    }
}
