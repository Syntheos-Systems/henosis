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
//! 5. Effective weight respects a per-attestor cap over a 30-day rolling window.
//! 6. Asymmetric delta: success raises, failure (harder) lowers, indeterminate
//!    is inert.
//! 7. After the last outcome, decay to `now`.
//! 8. Clamp to `[0.0, 1.0]`.

use std::collections::HashMap;

use syntheos_contracts::PrincipalId;
use time::{Duration, OffsetDateTime};

use crate::model::{
    Outcome, OutcomeAttestation, ATTESTOR_CAP, DECAY_RATE_PER_DAY, FAILURE_RATE, NEUTRAL_TRUST,
    SUCCESS_RATE, WEIGHT_NORMALIZATION,
};
use crate::room::RoomState;

/// Compute `target`'s trust score from the room's outcome history at `now`.
///
/// Returns `NEUTRAL_TRUST` if the target has no usable history. Pure: identical
/// inputs always produce identical output; `now` is always supplied by the
/// caller (never read from the wall clock here).
pub fn compute_trust(state: &RoomState, target: &PrincipalId, now: OffsetDateTime) -> f64 {
    let outcomes = match state.outcomes_by_target.get(target) {
        Some(v) if !v.is_empty() => v,
        _ => return NEUTRAL_TRUST,
    };

    // Sort by signed_at ascending for deterministic replay without mutating
    // the underlying storage.
    let mut sorted: Vec<&OutcomeAttestation> = outcomes.iter().collect();
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

        // Defense in depth: self-attestations should already be dropped at
        // receive time, but never rely on that being the only guard.
        if o.is_self_attestation() {
            continue;
        }

        let raw_weight = u32::from(o.clamped_weight());
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
    *score -= distance * DECAY_RATE_PER_DAY * days_elapsed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AdmittedPrincipal;
    use crate::room::RoomState;

    /// A fresh public key.
    fn pubkey() -> crate::crypto::PublicKey {
        crate::crypto::SecretKey::generate().0
    }

    /// A room with the listed principals admitted.
    fn admitted_state(principals: &[PrincipalId]) -> RoomState {
        let admits = principals
            .iter()
            .map(|p| AdmittedPrincipal::new(*p, pubkey(), vec![]))
            .collect();
        RoomState::from_genesis(Default::default(), [pubkey()].into_iter().collect(), admits)
    }

    /// An outcome attestation with explicit outcome, weight, and timestamp.
    fn outcome(
        target: PrincipalId,
        attestor: PrincipalId,
        outcome: Outcome,
        weight: u8,
        signed_at: OffsetDateTime,
    ) -> OutcomeAttestation {
        OutcomeAttestation {
            target,
            attestor,
            underlying_event_ref: "$evt:host".into(),
            outcome,
            weight,
            context: String::new(),
            signed_at,
        }
    }

    /// No history yields the neutral score.
    #[test]
    fn empty_history_returns_neutral() {
        let gir = PrincipalId::new();
        let s = admitted_state(&[gir]);
        let score = compute_trust(&s, &gir, OffsetDateTime::now_utc());
        assert!((score - NEUTRAL_TRUST).abs() < 1e-9);
    }

    /// A single success raises the score above neutral.
    #[test]
    fn one_success_increases_score() {
        let (gir, sec) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut s = admitted_state(&[gir, sec]);
        s.record_outcome(outcome(gir, sec, Outcome::Success, 5, now));
        assert!(compute_trust(&s, &gir, now) > NEUTRAL_TRUST);
    }

    /// A single failure lowers the score below neutral.
    #[test]
    fn one_failure_decreases_score() {
        let (gir, sec) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut s = admitted_state(&[gir, sec]);
        s.record_outcome(outcome(gir, sec, Outcome::Failure, 5, now));
        assert!(compute_trust(&s, &gir, now) < NEUTRAL_TRUST);
    }

    /// Failures hurt more than equal-weight successes help (FAILURE_RATE >
    /// SUCCESS_RATE).
    #[test]
    fn failures_hurt_more_than_successes_help() {
        let (gir, sec) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let mut succ = admitted_state(&[gir, sec]);
        succ.record_outcome(outcome(gir, sec, Outcome::Success, 5, now));
        let mut fail = admitted_state(&[gir, sec]);
        fail.record_outcome(outcome(gir, sec, Outcome::Failure, 5, now));
        let success_delta = compute_trust(&succ, &gir, now) - NEUTRAL_TRUST;
        let failure_delta = NEUTRAL_TRUST - compute_trust(&fail, &gir, now);
        assert!(failure_delta > success_delta);
    }

    /// Score decays toward neutral as query time advances past the event.
    #[test]
    fn score_decays_toward_neutral_over_time() {
        let (gir, sec) = (PrincipalId::new(), PrincipalId::new());
        let now = OffsetDateTime::now_utc();
        let then = now - Duration::days(60);
        let mut s = admitted_state(&[gir, sec]);
        s.record_outcome(outcome(gir, sec, Outcome::Success, 10, then));
        let at_event = compute_trust(&s, &gir, then);
        let at_now = compute_trust(&s, &gir, now);
        assert!((at_now - NEUTRAL_TRUST).abs() < (at_event - NEUTRAL_TRUST).abs());
    }

    /// The per-attestor cap bounds a single attestor's contribution: many maxed
    /// successes exceed a single one but stay strictly below 1.0.
    #[test]
    fn per_attestor_cap_limits_contribution() {
        let (gir, sec) = (PrincipalId::new(), PrincipalId::new());
        let base = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let query = base + Duration::milliseconds(25);

        let mut capped = admitted_state(&[gir, sec]);
        let mut t = base;
        for _ in 0..20 {
            capped.record_outcome(outcome(gir, sec, Outcome::Success, 10, t));
            t += Duration::milliseconds(1);
        }
        let capped_score = compute_trust(&capped, &gir, query);

        let mut single = admitted_state(&[gir, sec]);
        single.record_outcome(outcome(gir, sec, Outcome::Success, 10, base));
        let single_score = compute_trust(&single, &gir, query);

        assert!(capped_score > single_score);
        assert!(capped_score < 1.0);
    }

    /// A self-attestation pushed directly into storage still moves nothing.
    #[test]
    fn self_attestation_contributes_zero() {
        let gir = PrincipalId::new();
        let now = OffsetDateTime::now_utc();
        let mut s = admitted_state(&[gir]);
        // Bypass record_outcome's drop to prove compute_trust also skips it.
        s.outcomes_by_target.entry(gir).or_default().push(outcome(
            gir,
            gir,
            Outcome::Success,
            10,
            now,
        ));
        assert!((compute_trust(&s, &gir, now) - NEUTRAL_TRUST).abs() < 1e-9);
    }

    /// The computation is deterministic across repeated runs.
    #[test]
    fn deterministic_across_runs() {
        let (gir, sec) = (PrincipalId::new(), PrincipalId::new());
        let t = OffsetDateTime::now_utc();
        let mut s = admitted_state(&[gir, sec]);
        s.record_outcome(outcome(gir, sec, Outcome::Success, 5, t));
        let a = compute_trust(&s, &gir, t);
        let b = compute_trust(&s, &gir, t);
        assert!((a - b).abs() < f64::EPSILON);
    }
}
