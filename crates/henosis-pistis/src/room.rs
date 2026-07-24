//! Raw and verified Pistis room state.
//!
//! A [`RoomStateSource`](crate::gate::RoomStateSource) may return untrusted,
//! serialized snapshots. Authorization never consumes those snapshots directly.
//! [`RoomState::verify_for`] checks the full scoped signature chain against a
//! gate-owned [`RoomTrustStore`] and produces a non-Serde
//! [`VerifiedRoomState`].

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use syntheos_contracts::PrincipalId;

use crate::crypto::{PublicKey, SecretKey};
use crate::model::{AdmittedPrincipal, OutcomeAttestation, RoomManifest, RoomPolicy, RoomScope};
use crate::{PistisError, Result};

/// One gate-owned issuer pin and rollback floor.
#[derive(Debug, Clone)]
struct RoomTrustPin {
    /// Issuer allowed to sign this exact tenant/room manifest.
    issuer_pubkey: PublicKey,
    /// Oldest manifest generation the gate will accept.
    minimum_generation: u64,
}

/// Gate-owned issuer pins keyed by exact tenant and room scope.
///
/// This store is never supplied by the room-state source. That separation is
/// the root of trust that prevents a compromised materializer from generating
/// its own issuer and self-authorizing.
#[derive(Debug, Clone, Default)]
pub struct RoomTrustStore {
    /// Exact-scope trust pins.
    pins: HashMap<RoomScope, RoomTrustPin>,
}

/// Builds and queries gate-owned room trust pins.
impl RoomTrustStore {
    /// Construct an empty store that trusts no room.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pin an issuer and minimum accepted generation for one exact scope.
    pub fn pin(
        &mut self,
        scope: RoomScope,
        issuer_pubkey: PublicKey,
        minimum_generation: u64,
    ) -> Result<()> {
        if let Some(existing) = self.pins.get(&scope) {
            if existing.issuer_pubkey != issuer_pubkey {
                return Err(PistisError::InvalidRoomState(
                    "cannot replace a room issuer pin in place".into(),
                ));
            }
            if minimum_generation < existing.minimum_generation {
                return Err(PistisError::InvalidRoomState(
                    "cannot lower a room generation floor".into(),
                ));
            }
        }
        self.pins.insert(
            scope,
            RoomTrustPin {
                issuer_pubkey,
                minimum_generation,
            },
        );
        Ok(())
    }

    /// Return the pin for an exact scope.
    fn get(&self, scope: &RoomScope) -> Option<&RoomTrustPin> {
        self.pins.get(scope)
    }
}

/// A raw materialized room snapshot.
///
/// Raw snapshots may cross a serialization boundary and are never authoritative
/// on their own. Their manifest, admissions, and outcomes must all verify for a
/// gate-pinned scope before authorization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomState {
    /// Issuer-signed room roots and policy.
    manifest: RoomManifest,
    /// Root-signed principal admissions keyed by claimed principal.
    admitted: HashMap<PrincipalId, AdmittedPrincipal>,
    /// Principal-signed outcomes grouped by claimed target.
    outcomes_by_target: HashMap<PrincipalId, Vec<OutcomeAttestation>>,
}

/// Constructs, materializes, and verifies raw room snapshots.
impl RoomState {
    /// Construct a signed raw snapshot from trusted genesis inputs.
    pub fn from_genesis(
        scope: RoomScope,
        generation: u64,
        policy: RoomPolicy,
        room_root_pubkeys: BTreeSet<PublicKey>,
        issuer_key: &SecretKey,
        initial_admits: Vec<AdmittedPrincipal>,
    ) -> Result<Self> {
        let mut state = Self {
            manifest: RoomManifest::new(scope, generation, policy, room_root_pubkeys, issuer_key),
            admitted: HashMap::new(),
            outcomes_by_target: HashMap::new(),
        };
        for admit in initial_admits {
            state.admit(admit)?;
        }
        Ok(state)
    }

    /// Return the scope claimed by this raw snapshot.
    pub fn scope(&self) -> &RoomScope {
        &self.manifest.scope
    }

    /// Add or replace an admission after validating its manifest root and scope.
    ///
    /// This is an early materializer check only. Gate-owned issuer provenance is
    /// established later by [`Self::verify_for`].
    pub fn admit(&mut self, admit: AdmittedPrincipal) -> Result<()> {
        admit.verify_admission(&self.manifest.scope, &self.manifest.room_root_pubkeys)?;
        self.admitted.insert(admit.principal, admit);
        Ok(())
    }

    /// Record one valid, non-self, non-replayed outcome.
    ///
    /// This is an early materializer check. [`Self::verify_for`] repeats every
    /// validation before the outcome can affect authorization.
    pub fn record_outcome(&mut self, attestation: OutcomeAttestation) -> Result<()> {
        if attestation.is_self_attestation() {
            return Err(PistisError::InvalidRoomState(
                "self-attestations are not accepted".into(),
            ));
        }
        let admitted = self.admitted.get(&attestation.attestor).ok_or_else(|| {
            PistisError::InvalidRoomState("outcome attestor has no validated room admission".into())
        })?;
        attestation.verify_attestation(&self.manifest.scope, &admitted.principal_pubkey)?;

        let outcomes = self
            .outcomes_by_target
            .entry(attestation.target)
            .or_default();
        if outcomes.iter().any(|existing| {
            existing.attestor == attestation.attestor
                && existing.underlying_event_ref == attestation.underlying_event_ref
        }) {
            return Err(PistisError::InvalidRoomState(
                "duplicate attestor-target-event outcome".into(),
            ));
        }
        outcomes.push(attestation);
        Ok(())
    }

    /// Verify the complete snapshot for an exact gate-pinned scope.
    pub fn verify_for(
        &self,
        expected_scope: &RoomScope,
        trust_store: &RoomTrustStore,
    ) -> Result<VerifiedRoomState> {
        let pin = trust_store.get(expected_scope).ok_or_else(|| {
            PistisError::InvalidRoomState("no gate trust pin for requested room".into())
        })?;
        self.manifest
            .verify(expected_scope, &pin.issuer_pubkey, pin.minimum_generation)?;

        for (principal, admission) in &self.admitted {
            if principal != &admission.principal {
                return Err(PistisError::InvalidRoomState(
                    "admission map key does not match signed principal".into(),
                ));
            }
            admission.verify_admission(expected_scope, &self.manifest.room_root_pubkeys)?;
        }

        let mut observed_events = HashSet::new();
        for (target, outcomes) in &self.outcomes_by_target {
            for outcome in outcomes {
                if target != &outcome.target {
                    return Err(PistisError::InvalidRoomState(
                        "outcome map key does not match signed target".into(),
                    ));
                }
                if outcome.is_self_attestation() {
                    return Err(PistisError::InvalidRoomState(
                        "self-attestation found in raw room state".into(),
                    ));
                }
                let admission = self.admitted.get(&outcome.attestor).ok_or_else(|| {
                    PistisError::InvalidRoomState(
                        "outcome attestor has no verified admission".into(),
                    )
                })?;
                outcome.verify_attestation(expected_scope, &admission.principal_pubkey)?;
                let event_key = (
                    outcome.target,
                    outcome.attestor,
                    outcome.underlying_event_ref.as_str(),
                );
                if !observed_events.insert(event_key) {
                    return Err(PistisError::InvalidRoomState(
                        "duplicate attestor-target-event outcome".into(),
                    ));
                }
            }
        }

        Ok(VerifiedRoomState {
            scope: expected_scope.clone(),
            policy: self.manifest.policy,
            admitted: self.admitted.clone(),
            outcomes_by_target: self.outcomes_by_target.clone(),
        })
    }

    /// Inject an outcome only for tests that exercise full-chain verification.
    #[cfg(test)]
    pub(crate) fn inject_outcome_for_test(&mut self, attestation: OutcomeAttestation) {
        self.outcomes_by_target
            .entry(attestation.target)
            .or_default()
            .push(attestation);
    }
}

/// An immutable, fully verified room snapshot accepted by authorization.
///
/// Construction is private to [`RoomState::verify_for`], and this type has no
/// Serde implementation. Raw source data therefore cannot masquerade as verified
/// authority state.
#[derive(Debug, Clone)]
pub struct VerifiedRoomState {
    /// Exact verified scope.
    scope: RoomScope,
    /// Issuer-signed and range-validated policy.
    policy: RoomPolicy,
    /// Fully verified admissions.
    admitted: HashMap<PrincipalId, AdmittedPrincipal>,
    /// Fully verified outcomes.
    outcomes_by_target: HashMap<PrincipalId, Vec<OutcomeAttestation>>,
}

/// Exposes read-only inputs from a verified room snapshot.
impl VerifiedRoomState {
    /// Return the exact verified tenant/room scope.
    pub fn scope(&self) -> &RoomScope {
        &self.scope
    }

    /// Return the verified room policy.
    pub fn policy(&self) -> &RoomPolicy {
        &self.policy
    }

    /// Return true when the principal has a verified admission.
    pub fn is_admitted(&self, principal: &PrincipalId) -> bool {
        self.admitted.contains_key(principal)
    }

    /// Return a principal's fully verified admission.
    pub fn trusted_admission(&self, principal: &PrincipalId) -> Option<&AdmittedPrincipal> {
        self.admitted.get(principal)
    }

    /// Return verified outcomes targeting one principal.
    pub(crate) fn outcomes_for(&self, principal: &PrincipalId) -> Option<&[OutcomeAttestation]> {
        self.outcomes_by_target.get(principal).map(Vec::as_slice)
    }
}

/// Unit tests for scoped room trust-chain verification.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Outcome, OutcomeStatement};
    use syntheos_contracts::TenantId;
    use time::OffsetDateTime;

    /// Build a fixed test scope.
    fn scope(room: &str) -> RoomScope {
        RoomScope::new(TenantId::new(), room)
    }

    /// Construct one root-signed principal admission.
    fn admission(
        scope: &RoomScope,
        principal: PrincipalId,
        principal_key: &SecretKey,
        room_root_key: &SecretKey,
    ) -> AdmittedPrincipal {
        AdmittedPrincipal::new(
            scope.clone(),
            principal,
            principal_key.public_key(),
            room_root_key,
            vec![],
        )
    }

    /// Construct one scoped principal-signed success outcome.
    fn outcome(
        scope: &RoomScope,
        target: PrincipalId,
        attestor: PrincipalId,
        principal_key: &SecretKey,
    ) -> OutcomeAttestation {
        OutcomeAttestation::new(
            OutcomeStatement {
                scope: scope.clone(),
                target,
                attestor,
                underlying_event_ref: "$evt:host".into(),
                outcome: Outcome::Success,
                weight: 5,
                context: String::new(),
                signed_at: OffsetDateTime::now_utc(),
            },
            principal_key,
        )
    }

    /// Construct a raw state and matching independent gate trust pin.
    fn state_and_trust(
        scope: &RoomScope,
        generation: u64,
        principal: PrincipalId,
        issuer_key: &SecretKey,
        room_root_key: &SecretKey,
        principal_key: &SecretKey,
    ) -> (RoomState, RoomTrustStore) {
        let state = RoomState::from_genesis(
            scope.clone(),
            generation,
            RoomPolicy::default(),
            BTreeSet::from([room_root_key.public_key()]),
            issuer_key,
            vec![admission(scope, principal, principal_key, room_root_key)],
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust
            .pin(scope.clone(), issuer_key.public_key(), generation)
            .unwrap();
        (state, trust)
    }

    /// A matching issuer pin verifies the complete room state.
    #[test]
    fn verifies_matching_trust_chain() {
        let scope = scope("!room");
        let principal = PrincipalId::new();
        let (_, issuer_key) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let (state, trust) = state_and_trust(
            &scope,
            7,
            principal,
            &issuer_key,
            &room_root_key,
            &principal_key,
        );
        let verified = state.verify_for(&scope, &trust).unwrap();
        assert_eq!(verified.scope(), &scope);
        assert!(verified.is_admitted(&principal));
    }

    /// A source-generated issuer cannot replace the gate-owned issuer pin.
    #[test]
    fn rejects_source_generated_issuer() {
        let scope = scope("!room");
        let principal = PrincipalId::new();
        let (_, pinned_issuer) = SecretKey::generate();
        let (_, source_issuer) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let (state, _source_trust) = state_and_trust(
            &scope,
            1,
            principal,
            &source_issuer,
            &room_root_key,
            &principal_key,
        );
        let mut gate_trust = RoomTrustStore::new();
        gate_trust
            .pin(scope.clone(), pinned_issuer.public_key(), 1)
            .unwrap();
        assert!(state.verify_for(&scope, &gate_trust).is_err());
    }

    /// A signed manifest cannot be replayed under another room scope.
    #[test]
    fn rejects_cross_room_manifest_replay() {
        let signed_scope = scope("!signed");
        let requested_scope = RoomScope::new(signed_scope.tenant, "!other");
        let principal = PrincipalId::new();
        let (_, issuer_key) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let (state, _trust) = state_and_trust(
            &signed_scope,
            1,
            principal,
            &issuer_key,
            &room_root_key,
            &principal_key,
        );
        let mut trust = RoomTrustStore::new();
        trust
            .pin(requested_scope.clone(), issuer_key.public_key(), 1)
            .unwrap();
        assert!(state.verify_for(&requested_scope, &trust).is_err());
    }

    /// A manifest older than the gate rollback floor is rejected.
    #[test]
    fn rejects_stale_manifest_generation() {
        let scope = scope("!room");
        let principal = PrincipalId::new();
        let (_, issuer_key) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let (state, _trust) = state_and_trust(
            &scope,
            2,
            principal,
            &issuer_key,
            &room_root_key,
            &principal_key,
        );
        let mut trust = RoomTrustStore::new();
        trust
            .pin(scope.clone(), issuer_key.public_key(), 3)
            .unwrap();
        assert!(state.verify_for(&scope, &trust).is_err());
    }

    /// A room root cannot also serve as the admitted principal signing key.
    #[test]
    fn rejects_collapsed_root_and_principal_key() {
        let scope = scope("!room");
        let principal = PrincipalId::new();
        let (_, issuer_key) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let collapsed = AdmittedPrincipal::new(
            scope.clone(),
            principal,
            room_root_key.public_key(),
            &room_root_key,
            vec![],
        );
        let state = RoomState::from_genesis(
            scope,
            1,
            RoomPolicy::default(),
            BTreeSet::from([room_root_key.public_key()]),
            &issuer_key,
            vec![collapsed],
        );
        assert!(state.is_err());
    }

    /// Valid outcomes verify, while wrong signers and replays are rejected.
    #[test]
    fn enforces_outcome_signer_and_replay() {
        let scope = scope("!room");
        let target = PrincipalId::new();
        let attestor = PrincipalId::new();
        let (_, issuer_key) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let (_, wrong_key) = SecretKey::generate();
        let (mut state, trust) = state_and_trust(
            &scope,
            1,
            attestor,
            &issuer_key,
            &room_root_key,
            &principal_key,
        );
        assert!(state
            .record_outcome(outcome(&scope, target, attestor, &wrong_key))
            .is_err());
        let signed = outcome(&scope, target, attestor, &principal_key);
        state.record_outcome(signed.clone()).unwrap();
        assert!(state.record_outcome(signed).is_err());
        assert!(state.verify_for(&scope, &trust).is_ok());
    }

    /// Full verification catches replay injected past materializer checks.
    #[test]
    fn verification_rejects_injected_replay() {
        let scope = scope("!room");
        let target = PrincipalId::new();
        let attestor = PrincipalId::new();
        let (_, issuer_key) = SecretKey::generate();
        let (_, room_root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let (mut state, trust) = state_and_trust(
            &scope,
            1,
            attestor,
            &issuer_key,
            &room_root_key,
            &principal_key,
        );
        let signed = outcome(&scope, target, attestor, &principal_key);
        state.inject_outcome_for_test(signed.clone());
        state.inject_outcome_for_test(signed);
        assert!(state.verify_for(&scope, &trust).is_err());
    }
}
