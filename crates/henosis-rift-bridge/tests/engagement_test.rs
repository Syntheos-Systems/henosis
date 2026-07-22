//! Integration tests for the probabilistic engagement engine.

use henosis_rift_bridge::engagement::{EngagementEngine, EngagementInputs};

/// Baseline inputs used by the tests below.
fn inputs(base_chance: f64, directly_addressed: bool, turns: Option<u32>) -> EngagementInputs {
    EngagementInputs {
        base_chance,
        directly_addressed,
        turns_since_last_post: turns,
        peer_responses: 0,
        relevance: 1.0,
    }
}

/// Verifies direct addressing raises response probability above the skip
/// floor even for an agent that literally just posted.
#[test]
fn test_directly_addressed_has_high_probability() {
    let engine = EngagementEngine::default();
    let prob = engine.compute_probability(inputs(0.3, true, Some(0)));
    assert!(
        prob >= 0.15,
        "directly addressed should have notable probability, got {prob}"
    );
}

/// Verifies a directly addressed agent that never posted gets the full
/// boost instead of applying recency damping to an agent with no history.
#[test]
fn test_directly_addressed_fresh_agent_gets_full_boost() {
    let engine = EngagementEngine::default();
    let prob = engine.compute_probability(inputs(0.3, true, None));
    assert!(
        (prob - engine.direct_address_boost).abs() < 1e-9,
        "fresh addressed agent should get the full boost, got {prob}"
    );
}

/// Verifies response probability recovers as the agent stays quiet longer.
#[test]
fn test_recency_decay_reduces_probability() {
    let engine = EngagementEngine::default();
    let p0 = engine.compute_probability(inputs(0.3, false, Some(0)));
    let p2 = engine.compute_probability(inputs(0.3, false, Some(2)));
    let p4 = engine.compute_probability(inputs(0.3, false, Some(4)));
    assert!(
        p0 < p2,
        "probability should recover from decay: {p0} < {p2}"
    );
    assert!(
        p2 < p4,
        "probability should continue recovering: {p2} < {p4}"
    );
}

/// Verifies very recent low-probability agents stay below the auto-skip threshold.
#[test]
fn test_auto_skip_threshold() {
    let engine = EngagementEngine::default();
    // Just posted (turns_since_last_post = 0) with low base_chance should be very low.
    let prob = engine.compute_probability(inputs(0.1, false, Some(0)));
    assert!(
        prob < 0.05,
        "should be below skip threshold right after posting, got {prob}"
    );
}

/// Verifies peers answering the message damp an unaddressed agent so
/// additional voices on an already covered point become increasingly rare.
#[test]
fn test_peer_responses_damp_probability() {
    let engine = EngagementEngine::default();
    let fresh = inputs(0.5, false, None);
    let p_first = engine.compute_probability(fresh);
    let p_third = engine.compute_probability(EngagementInputs {
        peer_responses: 2,
        ..fresh
    });
    assert!(
        p_third < p_first * 0.25,
        "two peer responses should damp hard: {p_third} vs {p_first}"
    );
}

/// Verifies deterministic extremes for the engagement roll.
#[test]
fn test_roll_respects_probability() {
    let engine = EngagementEngine::default();
    let engaged = (0..100).filter(|_| engine.roll(0.0)).count();
    assert_eq!(engaged, 0);
    let engaged = (0..100).filter(|_| engine.roll(1.0)).count();
    assert_eq!(engaged, 100);
}
