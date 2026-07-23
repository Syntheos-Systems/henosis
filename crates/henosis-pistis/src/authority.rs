//! Capability authorization over a materialized room state.
//!
//! Verbatim port of `pistis-authority`, retyped onto [`PrincipalId`] and the
//! `time` crate. The decision is a pure function: admission, then trust
//! threshold, then per-requirement capability match. A denied decision is a
//! normal return value, not an error.

use serde::{Deserialize, Serialize};
use syntheos_contracts::PrincipalId;
use time::OffsetDateTime;

use crate::model::{ActionKind, NEUTRAL_TRUST};
use crate::room::VerifiedRoomState;
use crate::trust::compute_trust;

/// One capability requirement a caller wants to exercise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRequirement {
    /// Capability namespace, e.g. `fs_read`, `deploy`.
    pub name: String,
    /// The action kind the capability must authorize.
    pub action_kind: ActionKind,
}

/// Request body for a capability authorization check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityCheckRequest {
    /// Principal attempting the operation.
    pub principal: PrincipalId,
    /// Required capabilities for the operation.
    pub required: Vec<CapabilityRequirement>,
}

/// Decision returned by a capability authorization check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityCheckDecision {
    /// Whether every requested capability is authorized.
    pub allowed: bool,
    /// Requirements that were not satisfied.
    pub missing: Vec<CapabilityRequirement>,
    /// Trust score computed for the principal at check time.
    pub trust_score: f64,
    /// Human-readable denial reason, if denied.
    pub reason: Option<String>,
}

/// Authorize `request` against `state` at time `now`.
///
/// Denies (without panicking) when the principal is not admitted, when its
/// trust score is below the room threshold, or when any required capability is
/// missing or expired.
pub fn authorize_capabilities(
    state: &VerifiedRoomState,
    request: &CapabilityCheckRequest,
    now: OffsetDateTime,
) -> CapabilityCheckDecision {
    let Some(admitted) = state.trusted_admission(&request.principal) else {
        return CapabilityCheckDecision {
            allowed: false,
            missing: request.required.clone(),
            trust_score: NEUTRAL_TRUST,
            reason: Some(format!(
                "principal {} lacks a valid trusted admission",
                request.principal.as_uuid()
            )),
        };
    };

    let trust_score = compute_trust(state, &request.principal, now);
    let threshold = state.policy().trust_threshold;
    if !(0.0..=1.0).contains(&threshold) {
        return CapabilityCheckDecision {
            allowed: false,
            missing: request.required.clone(),
            trust_score,
            reason: Some(format!(
                "room trust threshold {threshold:?} is outside [0.0, 1.0]"
            )),
        };
    }

    if !trust_score.is_finite() || trust_score < threshold {
        return CapabilityCheckDecision {
            allowed: false,
            missing: request.required.clone(),
            trust_score,
            reason: Some(format!(
                "trust score {trust_score:.3} is below room threshold {threshold:.3}"
            )),
        };
    }

    let mut missing = Vec::new();
    for requirement in &request.required {
        let matched = admitted.admitted_capabilities.iter().any(|cap| {
            cap.name == requirement.name
                && cap.action_kinds.contains(&requirement.action_kind)
                && cap.is_valid(now)
        });
        if !matched {
            missing.push(requirement.clone());
        }
    }

    if missing.is_empty() {
        CapabilityCheckDecision {
            allowed: true,
            missing,
            trust_score,
            reason: None,
        }
    } else {
        CapabilityCheckDecision {
            allowed: false,
            missing,
            trust_score,
            reason: Some("missing capability requirement".to_owned()),
        }
    }
}

/// Unit tests for capability authorization and the trust floor.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SecretKey;
    use crate::model::{
        AdmittedPrincipal, Capability, Outcome, OutcomeAttestation, OutcomeStatement, RoomPolicy,
        RoomScope,
    };
    use crate::room::{RoomState, RoomTrustStore};
    use std::collections::BTreeSet;
    use syntheos_contracts::TenantId;
    use time::Duration;

    /// A capability with one action kind and optional expiry.
    fn cap(name: &str, kind: ActionKind, expires_at: Option<OffsetDateTime>) -> Capability {
        Capability {
            name: name.to_owned(),
            action_kinds: BTreeSet::from([kind]),
            granted_by: "operator".to_owned(),
            expires_at,
        }
    }

    /// Build and verify a room with one principal holding `caps`.
    fn state_with(principal: PrincipalId, caps: Vec<Capability>) -> VerifiedRoomState {
        let scope = RoomScope::new(TenantId::new(), "!authority");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let state = RoomState::from_genesis(
            scope.clone(),
            1,
            RoomPolicy::default(),
            BTreeSet::from([root_key.public_key()]),
            &issuer_key,
            vec![AdmittedPrincipal::new(
                scope.clone(),
                principal,
                principal_key.public_key(),
                &root_key,
                caps,
            )],
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust
            .pin(scope.clone(), issuer_key.public_key(), 1)
            .unwrap();
        state.verify_for(&scope, &trust).unwrap()
    }

    /// Build a raw room whose issuer-signed policy may be invalid.
    fn raw_state_with_policy(
        principal: PrincipalId,
        policy: RoomPolicy,
    ) -> (RoomState, RoomScope, RoomTrustStore) {
        let scope = RoomScope::new(TenantId::new(), "!authority");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let state = RoomState::from_genesis(
            scope.clone(),
            1,
            policy,
            BTreeSet::from([root_key.public_key()]),
            &issuer_key,
            vec![AdmittedPrincipal::new(
                scope.clone(),
                principal,
                principal_key.public_key(),
                &root_key,
                vec![cap("deploy", ActionKind::Deploy, None)],
            )],
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust
            .pin(scope.clone(), issuer_key.public_key(), 1)
            .unwrap();
        (state, scope, trust)
    }

    /// One requirement for `(name, kind)`.
    fn req(principal: PrincipalId, name: &str, kind: ActionKind) -> CapabilityCheckRequest {
        CapabilityCheckRequest {
            principal,
            required: vec![CapabilityRequirement {
                name: name.to_owned(),
                action_kind: kind,
            }],
        }
    }

    /// A held, valid capability is authorized.
    #[test]
    fn allows_held_capability() {
        let p = PrincipalId::new();
        let state = state_with(p, vec![cap("fs_read", ActionKind::Message, None)]);
        let d = authorize_capabilities(
            &state,
            &req(p, "fs_read", ActionKind::Message),
            OffsetDateTime::now_utc(),
        );
        assert!(d.allowed);
        assert!(d.missing.is_empty());
        assert!(d.reason.is_none());
    }

    /// A capability the principal lacks is denied and reported missing.
    #[test]
    fn denies_missing_capability() {
        let p = PrincipalId::new();
        let state = state_with(p, vec![cap("fs_read", ActionKind::Message, None)]);
        let d = authorize_capabilities(
            &state,
            &req(p, "bash", ActionKind::Commit),
            OffsetDateTime::now_utc(),
        );
        assert!(!d.allowed);
        assert_eq!(d.missing.len(), 1);
        assert!(d.reason.unwrap().contains("missing capability"));
    }

    /// An expired capability is treated as missing.
    #[test]
    fn denies_expired_capability() {
        let p = PrincipalId::new();
        let now = OffsetDateTime::now_utc();
        let state = state_with(
            p,
            vec![cap(
                "fs_read",
                ActionKind::Message,
                Some(now - Duration::seconds(1)),
            )],
        );
        let d = authorize_capabilities(&state, &req(p, "fs_read", ActionKind::Message), now);
        assert!(!d.allowed);
        assert_eq!(d.missing.len(), 1);
    }

    /// An unadmitted principal is denied without panicking.
    #[test]
    fn denies_unadmitted_principal() {
        let p = PrincipalId::new();
        let state = state_with(p, vec![cap("fs_read", ActionKind::Message, None)]);
        let intruder = PrincipalId::new();
        let d = authorize_capabilities(
            &state,
            &req(intruder, "fs_read", ActionKind::Message),
            OffsetDateTime::now_utc(),
        );
        assert!(!d.allowed);
        assert!(d.reason.unwrap().contains("valid trusted admission"));
    }

    /// A capable principal whose trust has been driven below threshold is denied
    /// on trust, before the capability even matters.
    #[test]
    fn denies_below_trust_threshold() {
        let p = PrincipalId::new();
        let attestor = PrincipalId::new();
        let scope = RoomScope::new(TenantId::new(), "!authority");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (principal_pubkey, _principal_key) = SecretKey::generate();
        let (attestor_pubkey, attestor_key) = SecretKey::generate();
        let mut state = RoomState::from_genesis(
            scope.clone(),
            1,
            RoomPolicy::default(),
            BTreeSet::from([root_key.public_key()]),
            &issuer_key,
            vec![
                AdmittedPrincipal::new(
                    scope.clone(),
                    p,
                    principal_pubkey,
                    &root_key,
                    vec![cap("deploy", ActionKind::Deploy, None)],
                ),
                AdmittedPrincipal::new(scope.clone(), attestor, attestor_pubkey, &root_key, vec![]),
            ],
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust
            .pin(scope.clone(), issuer_key.public_key(), 1)
            .unwrap();
        let now = OffsetDateTime::now_utc();
        // Hammer failures from a distinct attestor to push trust under 0.4.
        let mut t = now - Duration::days(1);
        for sequence in 0..10 {
            state
                .record_outcome(OutcomeAttestation::new(
                    OutcomeStatement {
                        scope: scope.clone(),
                        target: p,
                        attestor,
                        underlying_event_ref: format!("$e:{sequence}"),
                        outcome: Outcome::Failure,
                        weight: 10,
                        context: String::new(),
                        signed_at: t,
                    },
                    &attestor_key,
                ))
                .unwrap();
            t += Duration::seconds(1);
        }
        let verified = state.verify_for(&scope, &trust).unwrap();
        let d = authorize_capabilities(&verified, &req(p, "deploy", ActionKind::Deploy), now);
        assert!(
            !d.allowed,
            "below-threshold trust must deny a capable principal"
        );
        assert!(d.trust_score < verified.policy().trust_threshold);
        assert!(d.reason.unwrap().contains("below room threshold"));
    }

    /// A NaN trust threshold (e.g. from a corrupt/hostile materialized state)
    /// must fail closed, not silently bypass the trust floor via `score < NaN`.
    #[test]
    fn denies_on_nan_threshold() {
        let p = PrincipalId::new();
        let (state, scope, trust) = raw_state_with_policy(
            p,
            RoomPolicy {
                trust_threshold: f64::NAN,
            },
        );
        assert!(
            state.verify_for(&scope, &trust).is_err(),
            "NaN threshold must fail closed before authorization"
        );
    }

    /// A materializer cannot admit a principal under a root absent from the manifest.
    #[test]
    fn denies_untrusted_admission_root() {
        let p = PrincipalId::new();
        let scope = RoomScope::new(TenantId::new(), "!authority");
        let (_, issuer_key) = SecretKey::generate();
        let (_, trusted_key) = SecretKey::generate();
        let (_, untrusted_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let state = RoomState::from_genesis(
            scope.clone(),
            1,
            RoomPolicy::default(),
            BTreeSet::from([trusted_key.public_key()]),
            &issuer_key,
            vec![AdmittedPrincipal::new(
                scope,
                p,
                principal_key.public_key(),
                &untrusted_key,
                vec![cap("deploy", ActionKind::Deploy, None)],
            )],
        );
        assert!(state.is_err());
    }

    /// A materializer cannot add a capability after the trusted root signs admission.
    #[test]
    fn denies_tampered_admission_capabilities() {
        let p = PrincipalId::new();
        let scope = RoomScope::new(TenantId::new(), "!authority");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let (_, principal_key) = SecretKey::generate();
        let mut admitted = AdmittedPrincipal::new(
            scope.clone(),
            p,
            principal_key.public_key(),
            &root_key,
            vec![cap("fs_read", ActionKind::Message, None)],
        );
        admitted
            .admitted_capabilities
            .push(cap("deploy", ActionKind::Deploy, None));
        let state = RoomState::from_genesis(
            scope,
            1,
            RoomPolicy::default(),
            BTreeSet::from([root_key.public_key()]),
            &issuer_key,
            vec![admitted],
        );
        assert!(state.is_err());
    }

    /// Finite thresholds outside the policy domain fail closed.
    #[test]
    fn denies_thresholds_outside_unit_interval() {
        let p = PrincipalId::new();
        for threshold in [-0.1, 1.1, f64::INFINITY, f64::NEG_INFINITY] {
            let (state, scope, trust) = raw_state_with_policy(
                p,
                RoomPolicy {
                    trust_threshold: threshold,
                },
            );
            assert!(
                state.verify_for(&scope, &trust).is_err(),
                "threshold {threshold} must fail closed before authorization"
            );
        }
    }
}
