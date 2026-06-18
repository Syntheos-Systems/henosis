use henosis_rift_bridge::engagement::EngagementEngine;

/// Verifies direct addressing raises response probability above the skip floor.
#[test]
fn test_directly_addressed_has_high_probability() {
    let engine = EngagementEngine::default();
    let prob = engine.compute_probability(0.3, true, 0, 1.0);
    assert!(
        prob >= 0.15,
        "directly addressed should have notable probability, got {prob}"
    );
}

/// Verifies response probability recovers as the agent has been quiet longer.
#[test]
fn test_recency_decay_reduces_probability() {
    let engine = EngagementEngine::default();
    let p0 = engine.compute_probability(0.3, false, 0, 1.0);
    let p2 = engine.compute_probability(0.3, false, 2, 1.0);
    let p4 = engine.compute_probability(0.3, false, 4, 1.0);
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
    let prob = engine.compute_probability(0.1, false, 0, 1.0);
    assert!(
        prob < 0.05,
        "should be below skip threshold right after posting, got {prob}"
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
