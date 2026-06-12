//! Capability authorization over a materialized room state.
//!
//! Verbatim port of `pistis-authority`, retyped onto [`PrincipalId`] and the
//! `time` crate. The decision is a pure function: admission, then trust
//! threshold, then per-requirement capability match. A denied decision is a
//! normal return value, not an error.

use serde::{Deserialize, Serialize};
use syntheos_contracts::PrincipalId;
use time::OffsetDateTime;

use crate::model::ActionKind;
use crate::room::RoomState;
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
    state: &RoomState,
    request: &CapabilityCheckRequest,
    now: OffsetDateTime,
) -> CapabilityCheckDecision {
    let trust_score = compute_trust(state, &request.principal, now);

    let Some(admitted) = state.admitted.get(&request.principal) else {
        return CapabilityCheckDecision {
            allowed: false,
            missing: request.required.clone(),
            trust_score,
            reason: Some(format!(
                "principal {} is not admitted",
                request.principal.as_uuid()
            )),
        };
    };

    if trust_score < state.policy.trust_threshold {
        return CapabilityCheckDecision {
            allowed: false,
            missing: request.required.clone(),
            trust_score,
            reason: Some(format!(
                "trust score {trust_score:.3} is below room threshold {:.3}",
                state.policy.trust_threshold
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{PublicKey, SecretKey};
    use crate::model::{AdmittedPrincipal, Capability, Outcome, OutcomeAttestation, RoomPolicy};
    use std::collections::BTreeSet;
    use time::Duration;

    /// A fresh public key.
    fn pubkey() -> PublicKey {
        SecretKey::generate().0
    }

    /// A capability with one action kind and optional expiry.
    fn cap(name: &str, kind: ActionKind, expires_at: Option<OffsetDateTime>) -> Capability {
        Capability {
            name: name.to_owned(),
            action_kinds: BTreeSet::from([kind]),
            granted_by: "operator".to_owned(),
            expires_at,
        }
    }

    /// A room with one principal admitted holding `caps`.
    fn state_with(principal: PrincipalId, caps: Vec<Capability>) -> RoomState {
        RoomState::from_genesis(
            RoomPolicy::default(),
            [pubkey()].into_iter().collect(),
            vec![AdmittedPrincipal::new(principal, pubkey(), caps)],
        )
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
        assert!(d.reason.unwrap().contains("not admitted"));
    }

    /// A capable principal whose trust has been driven below threshold is denied
    /// on trust, before the capability even matters.
    #[test]
    fn denies_below_trust_threshold() {
        let p = PrincipalId::new();
        let attestor = PrincipalId::new();
        let mut state = state_with(p, vec![cap("deploy", ActionKind::Deploy, None)]);
        let now = OffsetDateTime::now_utc();
        // Hammer failures from a distinct attestor to push trust under 0.4.
        let mut t = now - Duration::days(1);
        for _ in 0..10 {
            state.record_outcome(OutcomeAttestation {
                target: p,
                attestor,
                underlying_event_ref: "$e".into(),
                outcome: Outcome::Failure,
                weight: 10,
                context: String::new(),
                signed_at: t,
            });
            t += Duration::seconds(1);
        }
        let d = authorize_capabilities(&state, &req(p, "deploy", ActionKind::Deploy), now);
        assert!(
            !d.allowed,
            "below-threshold trust must deny a capable principal"
        );
        assert!(d.trust_score < state.policy.trust_threshold);
        assert!(d.reason.unwrap().contains("below room threshold"));
    }
}
