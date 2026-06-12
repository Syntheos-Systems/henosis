//! The capability, admission, and trust-input model.
//!
//! An independent snapshot of `pistis-core`'s capability + outcome taxonomy,
//! reworked onto the Henosis principal model: every agent identity is a
//! [`PrincipalId`] and every timestamp is a [`time::OffsetDateTime`] (Henosis is
//! a `time`-crate codebase, not chrono). The signed-event machinery that
//! produces these values in Pistis is NOT absorbed -- Henosis consumes already
//! materialized values via the `RoomStateSource` seam.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use syntheos_contracts::PrincipalId;
use time::OffsetDateTime;

use crate::crypto::PublicKey;

// ---- trust-math constants (verbatim from pistis-core::outcome) ----

/// The neutral starting trust score (and decay target) for any principal.
pub const NEUTRAL_TRUST: f64 = 0.5;

/// Multiplier for a `Success` outcome's effect: `score += SUCCESS_RATE * (1 -
/// score) * effective_weight`.
pub const SUCCESS_RATE: f64 = 0.10;

/// Multiplier for a `Failure` outcome's effect:
/// `score -= FAILURE_RATE * score * effective_weight`. Asymmetric vs
/// `SUCCESS_RATE` -- failures hurt more than successes help, a design invariant.
pub const FAILURE_RATE: f64 = 0.15;

/// Per-day linear decay rate toward `NEUTRAL_TRUST` between outcome events.
pub const DECAY_RATE_PER_DAY: f64 = 0.02;

/// Per-attestor cumulative weighted contribution cap to one target's score
/// within a 30-day rolling window. Defends against attestor collusion.
pub const ATTESTOR_CAP: u32 = 50;

/// Divisor applied to raw `weight` values when computing effective weight.
pub const WEIGHT_NORMALIZATION: u32 = 10;

// ---- capability taxonomy ----

/// Every operation Pistis arbitrates falls under one of these kinds. Ordered to
/// match the Pistis design-doc risk table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ActionKind {
    /// Post a chat message in a room.
    Message,
    /// Declare currently-offered capabilities.
    CapabilityClaim,
    /// Attest to another agent's outcome on a verifiable event.
    Outcome,
    /// Accept a task from the coordination system.
    TaskAccept,
    /// Claim a task as complete.
    TaskComplete,
    /// Commit code to a non-protected branch.
    Commit,
    /// Commit code to a protected branch (main, release/*).
    CommitProtected,
    /// Merge a PR or branch to main.
    Merge,
    /// Deploy to production.
    Deploy,
    /// Delete data or files (any destructive operation).
    Delete,
    /// Rotate a credential, certificate, or signing key.
    CredentialRotate,
    /// Modify Pistis ledger state (admit, revoke, grant).
    LedgerModify,
    /// Sign a gatekeeper-review artifact.
    Review,
    /// Counter-sign another agent's pending action.
    Endorse,
}

impl ActionKind {
    /// Parse an action-kind token (the snake_case-ish names a tool invocation
    /// carries) into an `ActionKind`, or `None` if unrecognized.
    pub fn parse(token: &str) -> Option<Self> {
        let kind = match token {
            "message" => Self::Message,
            "capability_claim" => Self::CapabilityClaim,
            "outcome" => Self::Outcome,
            "task_accept" => Self::TaskAccept,
            "task_complete" => Self::TaskComplete,
            "commit" => Self::Commit,
            "commit_protected" => Self::CommitProtected,
            "merge" => Self::Merge,
            "deploy" => Self::Deploy,
            "delete" => Self::Delete,
            "credential_rotate" => Self::CredentialRotate,
            "ledger_modify" => Self::LedgerModify,
            "review" => Self::Review,
            "endorse" => Self::Endorse,
            _ => return None,
        };
        Some(kind)
    }
}

/// A named capability a principal holds: the set of `ActionKind`s it permits.
/// `name` is an opaque room-defined namespace (e.g. `rust-code`, `deploy`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    /// Capability name (room-defined namespace).
    pub name: String,
    /// Action kinds this capability permits.
    pub action_kinds: BTreeSet<ActionKind>,
    /// Who granted this capability (typically the operator, or empty).
    pub granted_by: String,
    /// Optional expiration. `None` means never expires.
    pub expires_at: Option<OffsetDateTime>,
}

impl Capability {
    /// Return true iff `now` is strictly before the expiration. A capability
    /// with `expires_at == Some(now)` is invalid (strict less-than). `None`
    /// never expires.
    pub fn is_valid(&self, now: OffsetDateTime) -> bool {
        match self.expires_at {
            None => true,
            Some(t) => now < t,
        }
    }
}

// ---- trust inputs ----

/// Outcome of an underlying interaction, as judged by an attestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Outcome {
    /// The interaction succeeded by the attestor's judgment.
    Success,
    /// The interaction failed by the attestor's judgment.
    Failure,
    /// The attestor cannot judge success/failure (informational).
    Indeterminate,
}

/// A signed attestation by `attestor` about `target`'s behavior on a verifiable
/// underlying event. Self-attestations (`target == attestor`) contribute zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeAttestation {
    /// Principal whose behavior is being attested.
    pub target: PrincipalId,
    /// Principal issuing the attestation.
    pub attestor: PrincipalId,
    /// Opaque reference to the underlying event being attested (audit aid).
    pub underlying_event_ref: String,
    /// Outcome judgment.
    pub outcome: Outcome,
    /// Raw weight in `[1, 10]`. Effective weight is computed by the trust math
    /// after the per-attestor cap and normalization.
    pub weight: u8,
    /// Free-form context explaining the attestation (audit aid).
    pub context: String,
    /// UTC timestamp of attestation.
    pub signed_at: OffsetDateTime,
}

impl OutcomeAttestation {
    /// Return true iff this is a self-attestation (`target == attestor`).
    pub fn is_self_attestation(&self) -> bool {
        self.target == self.attestor
    }

    /// Return the raw weight clamped to `[1, 10]`.
    pub fn clamped_weight(&self) -> u8 {
        self.weight.clamp(1, 10)
    }
}

// ---- admission + policy ----

/// A currently-admitted principal and the capabilities its admission grants.
///
/// The AdmitPayload essence, reworked onto the principal model. The persona /
/// session subkey bookkeeping Pistis tracks is not part of the capability
/// decision and is not absorbed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmittedPrincipal {
    /// The admitted principal.
    pub principal: PrincipalId,
    /// The principal's master public key (root of its signing chain).
    pub master_pubkey: PublicKey,
    /// Capabilities this principal may exercise.
    pub admitted_capabilities: Vec<Capability>,
}

impl AdmittedPrincipal {
    /// Construct an admitted principal.
    pub fn new(
        principal: PrincipalId,
        master_pubkey: PublicKey,
        admitted_capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            principal,
            master_pubkey,
            admitted_capabilities,
        }
    }
}

/// The room policy inputs the capability decision reads. Focused to the field
/// the gate uses; the counter-sign / cooling-off / risk-override policy that
/// Pistis also carries is not absorbed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoomPolicy {
    /// Minimum trust score for a principal's capabilities to be honored.
    pub trust_threshold: f64,
}

impl Default for RoomPolicy {
    /// The Pistis design-doc default trust threshold (0.4).
    fn default() -> Self {
        Self {
            trust_threshold: 0.4,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;

    /// `now` for tests.
    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    /// Build a non-expiring capability.
    fn cap(name: &str, kinds: &[ActionKind]) -> Capability {
        Capability {
            name: name.to_string(),
            action_kinds: kinds.iter().copied().collect(),
            granted_by: String::new(),
            expires_at: None,
        }
    }

    /// An outcome attestation between two principals.
    fn att(target: PrincipalId, attestor: PrincipalId, weight: u8) -> OutcomeAttestation {
        OutcomeAttestation {
            target,
            attestor,
            underlying_event_ref: "$evt:host".into(),
            outcome: Outcome::Success,
            weight,
            context: String::new(),
            signed_at: now(),
        }
    }

    /// The trust constants match the Pistis design doc, and the asymmetry
    /// invariant holds at compile time.
    #[test]
    fn constants_match_design_doc() {
        assert_eq!(NEUTRAL_TRUST, 0.5);
        assert_eq!(SUCCESS_RATE, 0.10);
        assert_eq!(FAILURE_RATE, 0.15);
        const { assert!(FAILURE_RATE > SUCCESS_RATE) }
        assert_eq!(DECAY_RATE_PER_DAY, 0.02);
        assert_eq!(ATTESTOR_CAP, 50);
        assert_eq!(WEIGHT_NORMALIZATION, 10);
    }

    /// A capability with no expiry is always valid.
    #[test]
    fn capability_valid_when_no_expiry() {
        assert!(cap("rust", &[ActionKind::Commit]).is_valid(now()));
    }

    /// A capability past its expiry is invalid (strict boundary).
    #[test]
    fn capability_invalid_when_past_expiry() {
        let mut c = cap("rust", &[ActionKind::Commit]);
        c.expires_at = Some(now() - Duration::seconds(1));
        assert!(!c.is_valid(now()));
    }

    /// A self-attestation is recognized; a cross-attestation is not.
    #[test]
    fn self_attestation_detected() {
        let a = PrincipalId::new();
        let b = PrincipalId::new();
        assert!(att(a, a, 5).is_self_attestation());
        assert!(!att(a, b, 5).is_self_attestation());
    }

    /// Weight is clamped into `[1, 10]`.
    #[test]
    fn weight_clamps() {
        let a = PrincipalId::new();
        let b = PrincipalId::new();
        assert_eq!(att(a, b, 7).clamped_weight(), 7);
        assert_eq!(att(a, b, 0).clamped_weight(), 1);
        assert_eq!(att(a, b, 200).clamped_weight(), 10);
    }

    /// Action-kind tokens round-trip through `parse`, and junk is rejected.
    #[test]
    fn action_kind_parses_known_tokens() {
        assert_eq!(ActionKind::parse("commit"), Some(ActionKind::Commit));
        assert_eq!(ActionKind::parse("deploy"), Some(ActionKind::Deploy));
        assert_eq!(
            ActionKind::parse("credential_rotate"),
            Some(ActionKind::CredentialRotate)
        );
        assert_eq!(ActionKind::parse("not_a_kind"), None);
    }

    /// The default room policy carries the design-doc trust threshold.
    #[test]
    fn default_policy_threshold() {
        assert_eq!(RoomPolicy::default().trust_threshold, 0.4);
    }
}
