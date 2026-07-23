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
use syntheos_contracts::{PrincipalId, TenantId};
use time::OffsetDateTime;

use crate::crypto::{PublicKey, SecretKey, Signature};

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

/// The exact tenant and room governed by a Pistis trust chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RoomScope {
    /// Tenant whose principals and authority records are in scope.
    pub tenant: TenantId,
    /// Stable room identifier inside the tenant.
    pub room: String,
}

/// Constructs and canonically encodes room scopes.
impl RoomScope {
    /// Construct an exact tenant/room scope.
    pub fn new(tenant: TenantId, room: impl Into<String>) -> Self {
        Self {
            tenant,
            room: room.into(),
        }
    }

    /// Append this scope to a canonical signing statement.
    fn append_signing_bytes(&self, target: &mut Vec<u8>) {
        target.extend_from_slice(self.tenant.as_uuid().as_bytes());
        append_bytes(target, self.room.as_bytes());
    }
}

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

/// Parses and canonically encodes action kinds.
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

    /// Return the stable byte code used by signed admission statements.
    fn admission_code(self) -> u8 {
        match self {
            Self::Message => 0,
            Self::CapabilityClaim => 1,
            Self::Outcome => 2,
            Self::TaskAccept => 3,
            Self::TaskComplete => 4,
            Self::Commit => 5,
            Self::CommitProtected => 6,
            Self::Merge => 7,
            Self::Deploy => 8,
            Self::Delete => 9,
            Self::CredentialRotate => 10,
            Self::LedgerModify => 11,
            Self::Review => 12,
            Self::Endorse => 13,
        }
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

/// Evaluates capability lifetime constraints.
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

/// Canonically encodes outcome judgments.
impl Outcome {
    /// Return the stable byte code used by signed outcome attestations.
    fn signing_code(self) -> u8 {
        match self {
            Self::Success => 0,
            Self::Failure => 1,
            Self::Indeterminate => 2,
        }
    }
}

/// The canonical claim an attestor signs before it can affect room trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeStatement {
    /// Exact tenant/room scope in which this claim is valid.
    pub scope: RoomScope,
    /// Principal whose behavior is being attested.
    pub target: PrincipalId,
    /// Principal issuing the attestation.
    pub attestor: PrincipalId,
    /// Opaque reference to the underlying event being attested (audit aid).
    pub underlying_event_ref: String,
    /// Outcome judgment.
    pub outcome: Outcome,
    /// Raw weight in `[1, 10]`.
    pub weight: u8,
    /// Free-form context explaining the attestation (audit aid).
    pub context: String,
    /// UTC timestamp of attestation.
    pub signed_at: OffsetDateTime,
}

/// A signed attestation by `attestor` about `target`'s behavior on a verifiable
/// underlying event. Self-attestations (`target == attestor`) contribute zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeAttestation {
    /// Exact tenant/room scope in which this attestation is valid.
    pub scope: RoomScope,
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
    /// Attestor-key signature over the canonical outcome statement.
    pub signature: Signature,
}

/// Signs, verifies, and inspects outcome attestations.
impl OutcomeAttestation {
    /// Construct and sign an outcome attestation under the attestor's key.
    pub fn new(statement: OutcomeStatement, attestor_key: &SecretKey) -> Self {
        let mut attestation = Self {
            scope: statement.scope,
            target: statement.target,
            attestor: statement.attestor,
            underlying_event_ref: statement.underlying_event_ref,
            outcome: statement.outcome,
            weight: statement.weight,
            context: statement.context,
            signed_at: statement.signed_at,
            signature: Signature { bytes: Vec::new() },
        };
        attestation.signature = attestor_key.sign(&attestation.signing_bytes());
        attestation
    }

    /// Verify this exact attestation under the admitted attestor public key.
    pub fn verify_attestation(
        &self,
        expected_scope: &RoomScope,
        attestor_key: &PublicKey,
    ) -> crate::Result<()> {
        if &self.scope != expected_scope {
            return Err(crate::PistisError::InvalidRoomState(
                "outcome scope does not match requested room".into(),
            ));
        }
        if !(1..=10).contains(&self.weight) {
            return Err(crate::PistisError::InvalidRoomState(format!(
                "outcome weight {} is outside 1..=10",
                self.weight
            )));
        }
        attestor_key.verify(&self.signing_bytes(), &self.signature)
    }

    /// Return true iff this is a self-attestation (`target == attestor`).
    pub fn is_self_attestation(&self) -> bool {
        self.target == self.attestor
    }

    /// Encode this attestation as a versioned, domain-separated statement.
    fn signing_bytes(&self) -> Vec<u8> {
        const DOMAIN: &[u8] = b"henosis-pistis-outcome-v1";

        let mut statement = Vec::new();
        append_bytes(&mut statement, DOMAIN);
        self.scope.append_signing_bytes(&mut statement);
        statement.extend_from_slice(self.target.as_uuid().as_bytes());
        statement.extend_from_slice(self.attestor.as_uuid().as_bytes());
        append_bytes(&mut statement, self.underlying_event_ref.as_bytes());
        statement.push(self.outcome.signing_code());
        statement.push(self.weight);
        append_bytes(&mut statement, self.context.as_bytes());
        statement.extend_from_slice(&self.signed_at.unix_timestamp_nanos().to_be_bytes());
        statement
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
    /// Exact tenant/room scope in which this admission is valid.
    pub scope: RoomScope,
    /// The admitted principal.
    pub principal: PrincipalId,
    /// Public key the principal uses to sign its own scoped attestations.
    pub principal_pubkey: PublicKey,
    /// Manifest-authorized room root that signed this admission.
    pub admission_root_pubkey: PublicKey,
    /// Capabilities this principal may exercise.
    pub admitted_capabilities: Vec<Capability>,
    /// Root-key signature over the canonical admission statement.
    pub admission_signature: Signature,
}

/// Signs and verifies scoped principal admissions.
impl AdmittedPrincipal {
    /// Construct an admission that binds a principal key under a room root.
    pub fn new(
        scope: RoomScope,
        principal: PrincipalId,
        principal_pubkey: PublicKey,
        admission_root_key: &SecretKey,
        admitted_capabilities: Vec<Capability>,
    ) -> Self {
        let mut admitted = Self {
            scope,
            principal,
            principal_pubkey,
            admission_root_pubkey: admission_root_key.public_key(),
            admitted_capabilities,
            admission_signature: Signature { bytes: Vec::new() },
        };
        admitted.admission_signature = admission_root_key.sign(&admitted.signing_bytes());
        admitted
    }

    /// Verify scope, key-role separation, root membership, and signature.
    pub fn verify_admission(
        &self,
        expected_scope: &RoomScope,
        room_roots: &BTreeSet<PublicKey>,
    ) -> crate::Result<()> {
        if &self.scope != expected_scope {
            return Err(crate::PistisError::InvalidRoomState(
                "admission scope does not match requested room".into(),
            ));
        }
        if self.principal_pubkey == self.admission_root_pubkey {
            return Err(crate::PistisError::InvalidRoomState(
                "principal and room-root signing keys must be distinct".into(),
            ));
        }
        if !room_roots.contains(&self.admission_root_pubkey) {
            return Err(crate::PistisError::InvalidRoomState(
                "admission signer is not a manifest-authorized room root".into(),
            ));
        }
        self.admission_root_pubkey
            .verify(&self.signing_bytes(), &self.admission_signature)
    }

    /// Encode this admission as a versioned, domain-separated signing statement.
    fn signing_bytes(&self) -> Vec<u8> {
        const DOMAIN: &[u8] = b"henosis-pistis-admission-v1";

        let mut statement = Vec::new();
        append_bytes(&mut statement, DOMAIN);
        self.scope.append_signing_bytes(&mut statement);
        statement.extend_from_slice(self.principal.as_uuid().as_bytes());
        statement.extend_from_slice(&self.principal_pubkey.bytes);
        statement.extend_from_slice(&self.admission_root_pubkey.bytes);

        let mut capabilities: Vec<Vec<u8>> = self
            .admitted_capabilities
            .iter()
            .map(Self::capability_signing_bytes)
            .collect();
        capabilities.sort_unstable();
        statement.extend_from_slice(&(capabilities.len() as u64).to_be_bytes());
        for capability in capabilities {
            append_bytes(&mut statement, &capability);
        }
        statement
    }

    /// Encode one capability without ambiguous string or collection boundaries.
    fn capability_signing_bytes(capability: &Capability) -> Vec<u8> {
        let mut encoded = Vec::new();
        append_bytes(&mut encoded, capability.name.as_bytes());
        encoded.extend_from_slice(&(capability.action_kinds.len() as u64).to_be_bytes());
        for action_kind in &capability.action_kinds {
            encoded.push(action_kind.admission_code());
        }
        append_bytes(&mut encoded, capability.granted_by.as_bytes());
        match capability.expires_at {
            Some(expires_at) => {
                encoded.push(1);
                encoded.extend_from_slice(&expires_at.unix_timestamp_nanos().to_be_bytes());
            }
            None => encoded.push(0),
        }
        encoded
    }
}

/// Append a byte string with a fixed-width length prefix.
fn append_bytes(target: &mut Vec<u8>, value: &[u8]) {
    target.extend_from_slice(&(value.len() as u64).to_be_bytes());
    target.extend_from_slice(value);
}

/// The room policy inputs the capability decision reads. Focused to the field
/// the gate uses; the counter-sign / cooling-off / risk-override policy that
/// Pistis also carries is not absorbed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoomPolicy {
    /// Minimum trust score for a principal's capabilities to be honored.
    pub trust_threshold: f64,
}

/// Supplies the conservative default room policy.
impl Default for RoomPolicy {
    /// The Pistis design-doc default trust threshold (0.4).
    fn default() -> Self {
        Self {
            trust_threshold: 0.4,
        }
    }
}

/// An issuer-signed declaration of the roots and policy for one room generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomManifest {
    /// Exact tenant/room scope governed by this manifest.
    pub scope: RoomScope,
    /// Monotonic room-state generation used for rollback protection.
    pub generation: u64,
    /// Capability and trust policy bound by the issuer signature.
    pub policy: RoomPolicy,
    /// Room roots permitted to sign principal admissions.
    pub room_root_pubkeys: BTreeSet<PublicKey>,
    /// Issuer key claimed by the manifest and pinned independently by the gate.
    pub issuer_pubkey: PublicKey,
    /// Issuer signature over the complete canonical manifest.
    pub signature: Signature,
}

/// Signs and verifies scoped room manifests.
impl RoomManifest {
    /// Construct and sign a room manifest under its installation issuer.
    pub fn new(
        scope: RoomScope,
        generation: u64,
        policy: RoomPolicy,
        room_root_pubkeys: BTreeSet<PublicKey>,
        issuer_key: &SecretKey,
    ) -> Self {
        let mut manifest = Self {
            scope,
            generation,
            policy,
            room_root_pubkeys,
            issuer_pubkey: issuer_key.public_key(),
            signature: Signature { bytes: Vec::new() },
        };
        manifest.signature = issuer_key.sign(&manifest.signing_bytes());
        manifest
    }

    /// Verify exact scope, issuer pin, generation floor, policy, and signature.
    pub fn verify(
        &self,
        expected_scope: &RoomScope,
        expected_issuer: &PublicKey,
        minimum_generation: u64,
    ) -> crate::Result<()> {
        if &self.scope != expected_scope {
            return Err(crate::PistisError::InvalidRoomState(
                "manifest scope does not match requested room".into(),
            ));
        }
        if &self.issuer_pubkey != expected_issuer {
            return Err(crate::PistisError::InvalidRoomState(
                "manifest issuer does not match gate trust pin".into(),
            ));
        }
        if self.generation < minimum_generation {
            return Err(crate::PistisError::InvalidRoomState(format!(
                "manifest generation {} is below required minimum {}",
                self.generation, minimum_generation
            )));
        }
        if !(0.0..=1.0).contains(&self.policy.trust_threshold) {
            return Err(crate::PistisError::InvalidRoomState(
                "manifest trust threshold is outside [0.0, 1.0]".into(),
            ));
        }
        self.issuer_pubkey
            .verify(&self.signing_bytes(), &self.signature)
    }

    /// Encode this manifest as a versioned, domain-separated statement.
    fn signing_bytes(&self) -> Vec<u8> {
        const DOMAIN: &[u8] = b"henosis-pistis-room-manifest-v1";

        let mut statement = Vec::new();
        append_bytes(&mut statement, DOMAIN);
        self.scope.append_signing_bytes(&mut statement);
        statement.extend_from_slice(&self.generation.to_be_bytes());
        statement.extend_from_slice(&self.policy.trust_threshold.to_bits().to_be_bytes());
        statement.extend_from_slice(&(self.room_root_pubkeys.len() as u64).to_be_bytes());
        for root in &self.room_root_pubkeys {
            statement.extend_from_slice(&root.bytes);
        }
        statement.extend_from_slice(&self.issuer_pubkey.bytes);
        statement
    }
}

/// Unit tests for capability, signature, and manifest contracts.
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

    /// Build a fixed scoped room identifier.
    fn scope(room: &str) -> RoomScope {
        RoomScope::new(TenantId::new(), room)
    }

    /// Sign an outcome attestation between two principals.
    fn att(
        scope: &RoomScope,
        target: PrincipalId,
        attestor: PrincipalId,
        weight: u8,
        key: &SecretKey,
    ) -> OutcomeAttestation {
        OutcomeAttestation::new(
            OutcomeStatement {
                scope: scope.clone(),
                target,
                attestor,
                underlying_event_ref: "$evt:host".into(),
                outcome: Outcome::Success,
                weight,
                context: String::new(),
                signed_at: now(),
            },
            key,
        )
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
        let scope = scope("!room");
        let a = PrincipalId::new();
        let b = PrincipalId::new();
        let (_, key) = SecretKey::generate();
        assert!(att(&scope, a, a, 5, &key).is_self_attestation());
        assert!(!att(&scope, a, b, 5, &key).is_self_attestation());
    }

    /// Signed weights outside `[1, 10]` are rejected rather than normalized.
    #[test]
    fn invalid_weight_is_rejected() {
        let scope = scope("!room");
        let a = PrincipalId::new();
        let b = PrincipalId::new();
        let (pubkey, key) = SecretKey::generate();
        assert!(
            att(&scope, a, b, 7, &key)
                .verify_attestation(&scope, &pubkey)
                .is_ok()
        );
        assert!(
            att(&scope, a, b, 0, &key)
                .verify_attestation(&scope, &pubkey)
                .is_err()
        );
        assert!(
            att(&scope, a, b, 200, &key)
                .verify_attestation(&scope, &pubkey)
                .is_err()
        );
    }

    /// Outcome signatures bind scope and every statement field to one signer.
    #[test]
    fn outcome_signature_binds_statement_and_signer() {
        let scope = scope("!room");
        let target = PrincipalId::new();
        let attestor = PrincipalId::new();
        let (attestor_pubkey, attestor_key) = SecretKey::generate();
        let (wrong_pubkey, _wrong_key) = SecretKey::generate();
        let signed = OutcomeAttestation::new(
            OutcomeStatement {
                scope: scope.clone(),
                target,
                attestor,
                underlying_event_ref: "$evt:host".into(),
                outcome: Outcome::Success,
                weight: 7,
                context: "verified".into(),
                signed_at: now(),
            },
            &attestor_key,
        );
        assert!(signed.verify_attestation(&scope, &attestor_pubkey).is_ok());
        assert!(signed.verify_attestation(&scope, &wrong_pubkey).is_err());
        assert!(
            signed
                .verify_attestation(&RoomScope::new(scope.tenant, "!other"), &attestor_pubkey)
                .is_err()
        );

        let mut tampered = signed;
        tampered.context.push_str("-forged");
        assert!(
            tampered
                .verify_attestation(&scope, &attestor_pubkey)
                .is_err()
        );
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

    /// An admission signature binds scope, both key roles, principal, and capabilities.
    #[test]
    fn admission_signature_binds_authority_statement() {
        let scope = scope("!room");
        let original = PrincipalId::new();
        let (root_pubkey, root_key) = SecretKey::generate();
        let (principal_pubkey, _principal_key) = SecretKey::generate();
        let admission = AdmittedPrincipal::new(
            scope.clone(),
            original,
            principal_pubkey,
            &root_key,
            vec![cap("deploy", &[ActionKind::Deploy])],
        );
        let roots = BTreeSet::from([root_pubkey]);
        assert!(admission.verify_admission(&scope, &roots).is_ok());

        let mut forged_principal = admission.clone();
        forged_principal.principal = PrincipalId::new();
        assert!(forged_principal.verify_admission(&scope, &roots).is_err());

        let mut forged_capabilities = admission;
        forged_capabilities
            .admitted_capabilities
            .push(cap("delete", &[ActionKind::Delete]));
        assert!(
            forged_capabilities
                .verify_admission(&scope, &roots)
                .is_err()
        );
    }

    /// Reordering an otherwise identical capability set preserves its signature.
    #[test]
    fn admission_signature_canonicalizes_capability_order() {
        let scope = scope("!room");
        let principal = PrincipalId::new();
        let (root_pubkey, root_key) = SecretKey::generate();
        let (principal_pubkey, _principal_key) = SecretKey::generate();
        let mut admission = AdmittedPrincipal::new(
            scope.clone(),
            principal,
            principal_pubkey,
            &root_key,
            vec![
                cap("deploy", &[ActionKind::Deploy]),
                cap("review", &[ActionKind::Review]),
            ],
        );
        admission.admitted_capabilities.reverse();
        assert!(
            admission
                .verify_admission(&scope, &BTreeSet::from([root_pubkey]))
                .is_ok()
        );
    }

    /// A manifest signature is exact-scope, exact-issuer, and generation bound.
    #[test]
    fn manifest_verification_binds_scope_issuer_and_generation() {
        let scope = scope("!room");
        let (issuer_pubkey, issuer_key) = SecretKey::generate();
        let (root_pubkey, _root_key) = SecretKey::generate();
        let manifest = RoomManifest::new(
            scope.clone(),
            8,
            RoomPolicy::default(),
            BTreeSet::from([root_pubkey]),
            &issuer_key,
        );
        assert!(manifest.verify(&scope, &issuer_pubkey, 8).is_ok());
        assert!(manifest.verify(&scope, &issuer_pubkey, 9).is_err());
        assert!(
            manifest
                .verify(&RoomScope::new(scope.tenant, "!other"), &issuer_pubkey, 8,)
                .is_err()
        );
        let (wrong_issuer, _wrong_key) = SecretKey::generate();
        assert!(manifest.verify(&scope, &wrong_issuer, 8).is_err());
    }

    /// The default room policy carries the design-doc trust threshold.
    #[test]
    fn default_policy_threshold() {
        assert_eq!(RoomPolicy::default().trust_threshold, 0.4);
    }
}
