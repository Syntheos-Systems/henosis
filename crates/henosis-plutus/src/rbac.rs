//! RBAC role and permission model for the Plutus policy authority.
//!
//! Implements an additive permission hierarchy:
//! `Viewer` < `Member` < `Admin` < `Owner`; `Billing` is an independent axis.

use std::fmt;
use std::str::FromStr;

/// A membership role within an org, controlling what actions a principal may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Full control: all permissions including org deletion and billing management.
    Owner,
    /// Elevated: everything Member has plus secret reads and org management.
    Admin,
    /// Standard: can execute agents, submit tasks, invoke tools, and manage memory.
    Member,
    /// Read-only: can search memory and read org information, nothing else.
    Viewer,
    /// Billing-only: can manage billing configuration only; cannot execute agents.
    Billing,
}

/// Display a `Role` as its canonical lowercase text (matches the DB column value).
impl fmt::Display for Role {
    /// Write the lowercase canonical role name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Role methods.
impl Role {
    /// Return the canonical text representation stored in the DB.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
            Role::Billing => "billing",
        }
    }
}

/// An error returned when a role string is not a recognized variant.
#[derive(Debug)]
pub struct RoleParseError(String);

/// Display a role-parse error.
impl fmt::Display for RoleParseError {
    /// Write the unrecognized role string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown role: {:?}", self.0)
    }
}

/// Parse a `Role` from its canonical text form.
impl FromStr for Role {
    /// Role-parse error.
    type Err = RoleParseError;

    /// Parse the canonical lowercase role name.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "owner" => Ok(Role::Owner),
            "admin" => Ok(Role::Admin),
            "member" => Ok(Role::Member),
            "viewer" => Ok(Role::Viewer),
            "billing" => Ok(Role::Billing),
            other => Err(RoleParseError(other.to_string())),
        }
    }
}

/// An action class a principal can be permitted to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    /// Read org metadata (name, status, members).
    OrgRead,
    /// Execute an agent or run an agent-level workflow.
    AgentExecute,
    /// Submit a Chiasm task for scheduling.
    TaskSubmit,
    /// Invoke any registered tool.
    ToolInvoke,
    /// Store a memory entry in the cognitive core.
    MemoryStore,
    /// Search stored memories.
    MemorySearch,
    /// Read a credential or secret from the Phylax store.
    SecretRead,
    /// Manage billing configuration and payment methods.
    BillingManage,
    /// Delete the org (irreversible).
    OrgDelete,
}

/// Return whether `role` is permitted to exercise `perm`.
///
/// Permission hierarchy (additive from bottom up):
/// - `Viewer`:  OrgRead, MemorySearch
/// - `Member`:  Viewer + AgentExecute, TaskSubmit, ToolInvoke, MemoryStore
/// - `Admin`:   Member + SecretRead, OrgDelete
/// - `Owner`:   Admin + BillingManage
/// - `Billing`: BillingManage only (independent axis; cannot execute agents)
///
/// Any unrecognized (`_`) arm is fail-closed (false) by the exhaustive match.
pub fn can(role: Role, perm: Permission) -> bool {
    match (role, perm) {
        // Viewer permissions.
        (Role::Viewer, Permission::OrgRead) => true,
        (Role::Viewer, Permission::MemorySearch) => true,

        // Member permissions: Viewer set plus execution.
        (Role::Member, Permission::OrgRead) => true,
        (Role::Member, Permission::MemorySearch) => true,
        (Role::Member, Permission::AgentExecute) => true,
        (Role::Member, Permission::TaskSubmit) => true,
        (Role::Member, Permission::ToolInvoke) => true,
        (Role::Member, Permission::MemoryStore) => true,

        // Admin permissions: Member set plus secret/org management.
        (Role::Admin, Permission::OrgRead) => true,
        (Role::Admin, Permission::MemorySearch) => true,
        (Role::Admin, Permission::AgentExecute) => true,
        (Role::Admin, Permission::TaskSubmit) => true,
        (Role::Admin, Permission::ToolInvoke) => true,
        (Role::Admin, Permission::MemoryStore) => true,
        (Role::Admin, Permission::SecretRead) => true,
        (Role::Admin, Permission::OrgDelete) => true,

        // Owner: everything.
        (Role::Owner, _) => true,

        // Billing: billing management only; cannot run agents or access secrets.
        (Role::Billing, Permission::BillingManage) => true,

        // All other combinations are denied (fail-closed).
        _ => false,
    }
}

/// Unit tests for role permissions and access checks.
#[cfg(test)]
mod tests {
    use super::*;

    /// Representative cells from the permission matrix are correct.
    #[test]
    fn permission_matrix() {
        // Viewer: can read, can search, cannot execute agents.
        assert!(can(Role::Viewer, Permission::OrgRead));
        assert!(can(Role::Viewer, Permission::MemorySearch));
        assert!(!can(Role::Viewer, Permission::AgentExecute));
        assert!(!can(Role::Viewer, Permission::TaskSubmit));
        assert!(!can(Role::Viewer, Permission::SecretRead));
        assert!(!can(Role::Viewer, Permission::BillingManage));

        // Member: can execute agents, cannot read secrets.
        assert!(can(Role::Member, Permission::AgentExecute));
        assert!(can(Role::Member, Permission::MemoryStore));
        assert!(!can(Role::Member, Permission::SecretRead));
        assert!(!can(Role::Member, Permission::BillingManage));

        // Admin: can read secrets, cannot manage billing.
        assert!(can(Role::Admin, Permission::SecretRead));
        assert!(can(Role::Admin, Permission::OrgDelete));
        assert!(!can(Role::Admin, Permission::BillingManage));

        // Billing: billing only, cannot execute agents.
        assert!(can(Role::Billing, Permission::BillingManage));
        assert!(!can(Role::Billing, Permission::AgentExecute));
        assert!(!can(Role::Billing, Permission::SecretRead));

        // Owner: everything.
        assert!(can(Role::Owner, Permission::OrgRead));
        assert!(can(Role::Owner, Permission::AgentExecute));
        assert!(can(Role::Owner, Permission::BillingManage));
        assert!(can(Role::Owner, Permission::OrgDelete));
        assert!(can(Role::Owner, Permission::SecretRead));
    }

    /// Role round-trips through its text form.
    #[test]
    fn role_roundtrip() {
        for role in [
            Role::Owner,
            Role::Admin,
            Role::Member,
            Role::Viewer,
            Role::Billing,
        ] {
            let s = role.to_string();
            let back: Role = s.parse().expect("valid role");
            assert_eq!(role, back);
        }
    }

    /// Unknown role strings are rejected.
    #[test]
    fn role_parse_rejects_unknown() {
        assert!("superadmin".parse::<Role>().is_err());
    }
}
