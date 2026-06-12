//! The materialized room state the capability decision reads.
//!
//! A focused snapshot of `pistis-room::state::RoomState`: just the fields the
//! capability + trust decision consults -- the policy, the trusted master keys,
//! the admitted principals, and the outcome history that feeds the trust math.
//! The grants / pending-actions / revocation / replay-counter bookkeeping of the
//! full Pistis room engine is not absorbed; in a live deployment the
//! `RoomStateSource` produces this struct by materializing the room's event log
//! inside Pistis.
//!
//! Keyed by [`PrincipalId`] via `HashMap` rather than `BTreeMap`: `PrincipalId`
//! is `Hash + Eq` but not `Ord`, and the trust math sorts outcomes by
//! `signed_at` internally, so map iteration order never affects a decision.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use syntheos_contracts::PrincipalId;

use crate::crypto::PublicKey;
use crate::model::{AdmittedPrincipal, OutcomeAttestation, RoomPolicy};

/// The materialized state of a Pistis-managed room, reduced to what the
/// capability decision needs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoomState {
    /// Room policy (carries the trust threshold).
    pub policy: RoomPolicy,
    /// Trusted master public keys (roots of the admission chains).
    pub master_pubkeys: BTreeSet<PublicKey>,
    /// Currently admitted principals, keyed by principal.
    pub admitted: HashMap<PrincipalId, AdmittedPrincipal>,
    /// Outcome attestations grouped by the principal they target.
    pub outcomes_by_target: HashMap<PrincipalId, Vec<OutcomeAttestation>>,
}

impl RoomState {
    /// Construct a `RoomState` from genesis inputs: the policy, the trusted
    /// master keys, and the initial set of admitted principals.
    pub fn from_genesis(
        policy: RoomPolicy,
        master_pubkeys: BTreeSet<PublicKey>,
        initial_admits: Vec<AdmittedPrincipal>,
    ) -> Self {
        let admitted = initial_admits
            .into_iter()
            .map(|a| (a.principal, a))
            .collect();
        Self {
            policy,
            master_pubkeys,
            admitted,
            outcomes_by_target: HashMap::new(),
        }
    }

    /// Admit (or re-admit, replacing) a principal.
    pub fn admit(&mut self, admit: AdmittedPrincipal) {
        self.admitted.insert(admit.principal, admit);
    }

    /// Record an outcome attestation against its target. Self-attestations are
    /// dropped at receive time (defense in depth; the trust math also skips
    /// them).
    pub fn record_outcome(&mut self, attestation: OutcomeAttestation) {
        if attestation.is_self_attestation() {
            return;
        }
        self.outcomes_by_target
            .entry(attestation.target)
            .or_default()
            .push(attestation);
    }

    /// Return true iff `principal` is currently admitted.
    pub fn is_admitted(&self, principal: &PrincipalId) -> bool {
        self.admitted.contains_key(principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SecretKey;
    use crate::model::Outcome;
    use time::OffsetDateTime;

    /// A fresh public key.
    fn pubkey() -> PublicKey {
        SecretKey::generate().0
    }

    /// An admitted principal with no capabilities.
    fn admit(principal: PrincipalId) -> AdmittedPrincipal {
        AdmittedPrincipal::new(principal, pubkey(), vec![])
    }

    /// An outcome attestation.
    fn outcome(target: PrincipalId, attestor: PrincipalId) -> OutcomeAttestation {
        OutcomeAttestation {
            target,
            attestor,
            underlying_event_ref: "$evt:host".into(),
            outcome: Outcome::Success,
            weight: 5,
            context: String::new(),
            signed_at: OffsetDateTime::now_utc(),
        }
    }

    /// A default room is empty.
    #[test]
    fn default_room_is_empty() {
        let s = RoomState::default();
        assert!(s.admitted.is_empty());
        assert!(s.outcomes_by_target.is_empty());
        assert!(s.master_pubkeys.is_empty());
    }

    /// Genesis seeds the initial admitted principals.
    #[test]
    fn from_genesis_admits_initial_principals() {
        let a = PrincipalId::new();
        let b = PrincipalId::new();
        let s = RoomState::from_genesis(
            RoomPolicy::default(),
            [pubkey()].into_iter().collect(),
            vec![admit(a), admit(b)],
        );
        assert!(s.is_admitted(&a));
        assert!(s.is_admitted(&b));
        assert!(!s.is_admitted(&PrincipalId::new()));
        assert_eq!(s.master_pubkeys.len(), 1);
    }

    /// A recorded cross-attestation lands under its target.
    #[test]
    fn record_outcome_groups_by_target() {
        let target = PrincipalId::new();
        let attestor = PrincipalId::new();
        let mut s = RoomState::default();
        s.record_outcome(outcome(target, attestor));
        assert_eq!(s.outcomes_by_target.get(&target).map(Vec::len), Some(1));
    }

    /// A self-attestation is dropped, not stored.
    #[test]
    fn record_outcome_drops_self_attestation() {
        let p = PrincipalId::new();
        let mut s = RoomState::default();
        s.record_outcome(outcome(p, p));
        assert!(!s.outcomes_by_target.contains_key(&p));
    }
}
