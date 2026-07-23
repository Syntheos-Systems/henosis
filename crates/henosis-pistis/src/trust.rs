//! Trust-score computation.
//!
//! A deterministic score in `[0.0, 1.0]` derived from a principal's signed
//! outcome history. No mutable score is stored anywhere -- every read recomputes
//! from scratch. Verbatim port of `pistis-room::trust`, retyped onto
//! [`PrincipalId`] and the `time` crate.
//!
//! Algorithm:
//! 1. Start at `NEUTRAL_TRUST` (0.5).
//! 2. Walk attestations targeting the principal in `signed_at` order.
//! 3. Before each outcome, apply decay toward neutral over elapsed days.
//! 4. Skip self-attestations.
//! 5. Drop replayed or conflicting attestor-target-event references.
//! 6. Effective weight respects a per-attestor cap over a 30-day rolling window.
//! 7. Asymmetric delta: success raises, failure (harder) lowers, indeterminate
//!    is inert.
//! 8. After the last outcome, decay to `now`.
//! 9. Clamp to `[0.0, 1.0]`.

use std::collections::HashMap;

use syntheos_contracts::PrincipalId;
use time::{Duration, OffsetDateTime};

use crate::model::{
    ATTESTOR_CAP, DECAY_RATE_PER_DAY, FAILURE_RATE, NEUTRAL_TRUST, Outcome, OutcomeAttestation,
    SUCCESS_RATE, WEIGHT_NORMALIZATION,
};
use crate::room::VerifiedRoomState;

/// Compute `target`'s trust score from the room's outcome history at `now`.
///
/// Returns `NEUTRAL_TRUST` if the target has no usable history. Pure: identical
/// inputs always produce identical output; `now` is always supplied by the
/// caller (never read from the wall clock here).
pub fn compute_trust(state: &VerifiedRoomState, target: &PrincipalId, now: OffsetDateTime) -> f64 {
    let outcomes = match state.outcomes_for(target) {
        Some(v) if !v.is_empty() => v,
        _ => return NEUTRAL_TRUST,
    };

    let mut event_counts: HashMap<(PrincipalId, &str), usize> = HashMap::new();
    for outcome in outcomes {
        *event_counts
            .entry((outcome.attestor, outcome.underlying_event_ref.as_str()))
            .or_default() += 1;
    }

    // Filter before choosing the replay timeline. An untrusted or replayed
    // attestation must not affect either score deltas or decay intervals.
    let mut sorted: Vec<&OutcomeAttestation> = outcomes
        .iter()
        .filter(|outcome| {
            outcome.target == *target
                && !outcome.is_self_attestation()
                && outcome.signed_at <= now
                && event_counts.get(&(outcome.attestor, outcome.underlying_event_ref.as_str()))
                    == Some(&1)
                && state
                    .trusted_admission(&outcome.attestor)
                    .is_some_and(|admitted| {
                        outcome
                            .verify_attestation(state.scope(), &admitted.principal_pubkey)
                            .is_ok()
                    })
        })
        .collect();
    if sorted.is_empty() {
        return NEUTRAL_TRUST;
    }
    sorted.sort_by_key(|o| o.signed_at);

    let mut score: f64 = NEUTRAL_TRUST;
    let mut last_updated: OffsetDateTime = sorted[0].signed_at;
    // Per-attestor cumulative raw weight within a rolling 30-day window ending
    // at each outcome's signed_at; expired entries are pruned as we advance.
    let mut attestor_contribution: HashMap<PrincipalId, Vec<(OffsetDateTime, u32)>> =
        HashMap::new();

    for o in &sorted {
        apply_decay(&mut score, last_updated, o.signed_at);
        last_updated = o.signed_at;

        let raw_weight = u32::from(o.weight);
        let window_start = o.signed_at - Duration::days(30);
        let history = attestor_contribution.entry(o.attestor).or_default();
        history.retain(|(t, _)| *t >= window_start);
        let already_contributed: u32 = history.iter().map(|(_, w)| w).sum();
        let remaining_cap = ATTESTOR_CAP.saturating_sub(already_contributed);
        if remaining_cap == 0 {
            continue;
        }
        let effective_raw = raw_weight.min(remaining_cap);
        history.push((o.signed_at, effective_raw));

        // Normalize: effective_weight carries both the cap clamp and the
        // original raw weight, so high-confidence attestors still contribute
        // proportionally more within their cap.
        let effective_weight =
            (effective_raw as f64) * (raw_weight as f64) / (WEIGHT_NORMALIZATION as f64);

        match o.outcome {
            Outcome::Success => score += SUCCESS_RATE * (1.0 - score) * effective_weight,
            Outcome::Failure => score -= FAILURE_RATE * score * effective_weight,
            Outcome::Indeterminate => {}
        }

        // Clamp after each delta so float drift cannot escape [0, 1] mid-walk.
        score = score.clamp(0.0, 1.0);
    }

    apply_decay(&mut score, last_updated, now);
    score.clamp(0.0, 1.0)
}

/// Apply linear decay of `score` toward `NEUTRAL_TRUST` over the days elapsed
/// between `from` and `to`. No-op if `to` is not strictly after `from`.
fn apply_decay(score: &mut f64, from: OffsetDateTime, to: OffsetDateTime) {
    if to <= from {
        return;
    }
    let days_elapsed = (to - from).whole_milliseconds() as f64 / 1000.0 / 60.0 / 60.0 / 24.0;
    if days_elapsed <= 0.0 {
        return;
    }
    let distance = *score - NEUTRAL_TRUST;
    let progress = (DECAY_RATE_PER_DAY * days_elapsed).min(1.0);
    *score -= distance * progress;
}

/// Unit tests for trust scoring, decay, and contribution caps.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SecretKey;
    use crate::model::{AdmittedPrincipal, OutcomeStatement, RoomPolicy, RoomScope};
    use crate::room::{RoomState, RoomTrustStore};
    use std::collections::{BTreeSet, HashMap};
    use syntheos_contracts::TenantId;

    /// A raw test room, its independent gate trust, and principal signing keys.
    struct TestRoom {
        /// Raw source-controlled room snapshot.
        state: RoomState,
        /// Independent gate-owned issuer pin.
        trust: RoomTrustStore,
        /// Exact scope shared by every signed statement.
        scope: RoomScope,
        /// Principal signing keys used by outcome fixtures.
        keys: HashMap<PrincipalId, SecretKey>,
    }

    /// Verifies test-room snapshots before trust computation.
    impl TestRoom {
        /// Produce the only state type accepted by `compute_trust`.
        fn verified(&self) -> VerifiedRoomState {
            self.state.verify_for(&self.scope, &self.trust).unwrap()
        }
    }

    /// Build a room with independently rooted admissions for the listed principals.
    fn admitted_state(principals: &[PrincipalId]) -> TestRoom {
        let scope = RoomScope::new(TenantId::new(), "!trust");
        let (_, issuer_key) = SecretKey::generate();
        let (_, root_key) = SecretKey::generate();
        let mut principals_and_keys = Vec::new();
        for principal in principals {
            let (_, principal_key) = SecretKey::generate();
            principals_and_keys.push((*principal, principal_key));
        }
        let admits = principals_and_keys
            .iter()
            .map(|(principal, key)| {
                AdmittedPrincipal::new(
                    scope.clone(),
                    *principal,
                    key.public_key(),
                    &root_key,
                    vec![],
                )
            })
            .collect();
        let state = RoomState::from_genesis(
            scope.clone(),
            1,
            RoomPolicy::default(),
            BTreeSet::from([root_key.public_key()]),
            &issuer_key,
            admits,
        )
        .unwrap();
        let mut trust = RoomTrustStore::new();
        trust
            .pin(scope.clone(), issuer_key.public_key(), 1)
            .unwrap();
        TestRoom {
            state,
            trust,
            scope,
            keys: principals_and_keys.into_iter().collect(),
        }
    }

    /// An outcome attestation with explicit outcome, weight, and timestamp.
    fn outcome(
        scope: &RoomScope,
        target: PrincipalId,
        attestor: PrincipalId,
        outcome: Outcome,
        weight: u8,
        signed_at: OffsetDateTime,
        attestor_key: &SecretKey,
    ) -> OutcomeAttestation {
        OutcomeAttestation::new(
            OutcomeStatement {
                scope: scope.clone(),
                target,
                attestor,
                underlying_event_ref: format!("$evt:{}", signed_at.unix_timestamp_nanos()),
                outcome,
                weight,
                context: String::new(),
                signed_at,
            },
            attestor_key,
        )
    }

    /// No history yields the neutral score.
    #[test]
    fn empty_history_returns_neutral() {
        let target = PrincipalId::new();
        let room = admitted_state(&[target]);
        let score = compute_trust(&room.verified(), &target, OffsetDateTime::now_utc());
        assert!((score - NEUTRAL_TRUST).abs() < 1e-9);
    }

    /// A single success raises the score above neutral.
    #[test]
    fn one_success_increases_score() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target, attestor]);
        room.state
            .record_outcome(outcome(
                &room.scope,
                target,
                attestor,
                Outcome::Success,
                5,
                now,
                room.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        assert!(compute_trust(&room.verified(), &target, now) > NEUTRAL_TRUST);
    }

    /// A single failure lowers the score below neutral.
    #[test]
    fn one_failure_decreases_score() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target, attestor]);
        room.state
            .record_outcome(outcome(
                &room.scope,
                target,
                attestor,
                Outcome::Failure,
                5,
                now,
                room.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        assert!(compute_trust(&room.verified(), &target, now) < NEUTRAL_TRUST);
    }

    /// Failures hurt more than equal-weight successes help (FAILURE_RATE >
    /// SUCCESS_RATE).
    #[test]
    fn failures_hurt_more_than_successes_help() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut success = admitted_state(&[target, attestor]);
        success
            .state
            .record_outcome(outcome(
                &success.scope,
                target,
                attestor,
                Outcome::Success,
                5,
                now,
                success.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let mut failure = admitted_state(&[target, attestor]);
        failure
            .state
            .record_outcome(outcome(
                &failure.scope,
                target,
                attestor,
                Outcome::Failure,
                5,
                now,
                failure.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let success_delta = compute_trust(&success.verified(), &target, now) - NEUTRAL_TRUST;
        let failure_delta = NEUTRAL_TRUST - compute_trust(&failure.verified(), &target, now);
        assert!(failure_delta > success_delta);
    }

    /// Score decays toward neutral as query time advances past the event.
    #[test]
    fn score_decays_toward_neutral_over_time() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let then = now - Duration::days(60);
        let mut room = admitted_state(&[target, attestor]);
        room.state
            .record_outcome(outcome(
                &room.scope,
                target,
                attestor,
                Outcome::Success,
                10,
                then,
                room.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let state = room.verified();
        let at_event = compute_trust(&state, &target, then);
        let at_now = compute_trust(&state, &target, now);
        assert!((at_now - NEUTRAL_TRUST).abs() < (at_event - NEUTRAL_TRUST).abs());
    }

    /// Long decay reaches neutral without crossing to the opposite side.
    #[test]
    fn long_decay_never_crosses_neutral() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let event_time = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();

        let mut success = admitted_state(&[target, attestor]);
        success
            .state
            .record_outcome(outcome(
                &success.scope,
                target,
                attestor,
                Outcome::Success,
                10,
                event_time,
                success.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let success_score = compute_trust(
            &success.verified(),
            &target,
            event_time + Duration::days(500),
        );
        assert_eq!(success_score, NEUTRAL_TRUST);

        let mut failure = admitted_state(&[target, attestor]);
        failure
            .state
            .record_outcome(outcome(
                &failure.scope,
                target,
                attestor,
                Outcome::Failure,
                10,
                event_time,
                failure.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let failure_score = compute_trust(
            &failure.verified(),
            &target,
            event_time + Duration::days(500),
        );
        assert_eq!(failure_score, NEUTRAL_TRUST);
    }

    /// The per-attestor cap bounds a single attestor's contribution: many maxed
    /// successes exceed a single one but stay strictly below 1.0.
    #[test]
    fn per_attestor_cap_limits_contribution() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let query = base + Duration::milliseconds(25);

        let mut capped = admitted_state(&[target, attestor]);
        let mut t = base;
        for _ in 0..20 {
            capped
                .state
                .record_outcome(outcome(
                    &capped.scope,
                    target,
                    attestor,
                    Outcome::Success,
                    10,
                    t,
                    capped.keys.get(&attestor).unwrap(),
                ))
                .unwrap();
            t += Duration::milliseconds(1);
        }
        let capped_score = compute_trust(&capped.verified(), &target, query);

        let mut single = admitted_state(&[target, attestor]);
        single
            .state
            .record_outcome(outcome(
                &single.scope,
                target,
                attestor,
                Outcome::Success,
                10,
                base,
                single.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let single_score = compute_trust(&single.verified(), &target, query);

        assert!(capped_score > single_score);
        assert!(capped_score < 1.0);
    }

    /// A self-attestation injected past ingestion cannot become verified state.
    #[test]
    fn self_attestation_prevents_verification() {
        let target = PrincipalId::new();
        let now = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target]);
        room.state.inject_outcome_for_test(outcome(
            &room.scope,
            target,
            target,
            Outcome::Success,
            10,
            now,
            room.keys.get(&target).unwrap(),
        ));
        assert!(room.state.verify_for(&room.scope, &room.trust).is_err());
    }

    /// Unadmitted outcomes and admissions under foreign roots are rejected.
    #[test]
    fn untrusted_attestors_are_rejected() {
        let target = PrincipalId::new();
        let unadmitted = PrincipalId::new();
        let untrusted = PrincipalId::new();
        let now = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target]);
        let (_, untrusted_principal_key) = crate::crypto::SecretKey::generate();
        let (_untrusted_pubkey, untrusted_key) = crate::crypto::SecretKey::generate();
        assert!(
            room.state
                .admit(AdmittedPrincipal::new(
                    room.scope.clone(),
                    untrusted,
                    untrusted_principal_key.public_key(),
                    &untrusted_key,
                    vec![],
                ))
                .is_err()
        );
        assert!(
            room.state
                .record_outcome(outcome(
                    &room.scope,
                    target,
                    unadmitted,
                    Outcome::Success,
                    10,
                    now,
                    &untrusted_key,
                ))
                .is_err()
        );
        assert_eq!(compute_trust(&room.verified(), &target, now), NEUTRAL_TRUST);
    }

    /// A future-dated attestation has no effect before its signed instant.
    #[test]
    fn future_attestation_does_not_affect_current_trust() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let mut room = admitted_state(&[target, attestor]);
        room.state
            .record_outcome(outcome(
                &room.scope,
                target,
                attestor,
                Outcome::Success,
                10,
                now + Duration::days(1),
                room.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let state = room.verified();
        assert_eq!(compute_trust(&state, &target, now), NEUTRAL_TRUST);
        assert!(compute_trust(&state, &target, now + Duration::days(1)) > NEUTRAL_TRUST);
    }

    /// The computation is deterministic across repeated runs.
    #[test]
    fn deterministic_across_runs() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let t = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target, attestor]);
        room.state
            .record_outcome(outcome(
                &room.scope,
                target,
                attestor,
                Outcome::Success,
                5,
                t,
                room.keys.get(&attestor).unwrap(),
            ))
            .unwrap();
        let state = room.verified();
        let a = compute_trust(&state, &target, t);
        let b = compute_trust(&state, &target, t);
        assert!((a - b).abs() < f64::EPSILON);
    }

    /// Full-chain verification rejects a forged outcome injected past ingestion.
    #[test]
    fn forged_attestation_prevents_verification() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target, attestor]);
        let (_wrong_pubkey, wrong_key) = SecretKey::generate();
        room.state.inject_outcome_for_test(outcome(
            &room.scope,
            target,
            attestor,
            Outcome::Success,
            10,
            now,
            &wrong_key,
        ));
        assert!(room.state.verify_for(&room.scope, &room.trust).is_err());

        let mut clean_room = admitted_state(&[target, attestor]);
        let mut tampered = outcome(
            &clean_room.scope,
            target,
            attestor,
            Outcome::Success,
            10,
            now,
            clean_room.keys.get(&attestor).unwrap(),
        );
        tampered.underlying_event_ref.push_str("-forged");
        clean_room.state.inject_outcome_for_test(tampered);
        assert!(
            clean_room
                .state
                .verify_for(&clean_room.scope, &clean_room.trust)
                .is_err()
        );
    }

    /// Full-chain verification rejects every ambiguously replayed event.
    #[test]
    fn replayed_attestation_prevents_verification() {
        let (target, attestor) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut room = admitted_state(&[target, attestor]);
        let signed = outcome(
            &room.scope,
            target,
            attestor,
            Outcome::Success,
            10,
            now,
            room.keys.get(&attestor).unwrap(),
        );
        room.state.inject_outcome_for_test(signed.clone());
        room.state.inject_outcome_for_test(signed);
        assert!(room.state.verify_for(&room.scope, &room.trust).is_err());
    }
}
