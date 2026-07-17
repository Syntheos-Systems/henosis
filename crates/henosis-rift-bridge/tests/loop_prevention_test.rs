use henosis_rift_bridge::loop_prevention::LoopGuard;
use henosis_rift_bridge::types::AgentId;
use uuid::Uuid;

/// Verifies each agent is blocked after exhausting its per-topic turn budget.
#[test]
fn test_turn_budget_enforced() {
    let mut guard = LoopGuard::new(3, 10);
    let agent = AgentId(Uuid::new_v4());

    assert!(guard.can_contribute(agent));
    guard.record_contribution(agent);
    guard.record_contribution(agent);
    guard.record_contribution(agent);
    assert!(
        !guard.can_contribute(agent),
        "should be blocked after 3 turns"
    );
}

/// Verifies the global thread ceiling blocks every agent once reached.
#[test]
fn test_thread_ceiling_enforced() {
    let mut guard = LoopGuard::new(100, 5);
    let agent_a = AgentId(Uuid::new_v4());
    let agent_b = AgentId(Uuid::new_v4());

    for _ in 0..3 {
        guard.record_contribution(agent_a);
    }
    for _ in 0..2 {
        guard.record_contribution(agent_b);
    }

    assert!(!guard.can_contribute(agent_a), "thread ceiling reached");
    assert!(!guard.can_contribute(agent_b), "thread ceiling reached");
}

/// Verifies consensus is true only after every registered agent agrees.
#[test]
fn test_agreement_tracking() {
    let mut guard = LoopGuard::new(5, 30);
    let a = AgentId(Uuid::new_v4());
    let b = AgentId(Uuid::new_v4());
    let c = AgentId(Uuid::new_v4());

    guard.register_agent(a);
    guard.register_agent(b);
    guard.register_agent(c);

    assert!(!guard.has_consensus());
    guard.record_agreement(a);
    guard.record_agreement(b);
    assert!(!guard.has_consensus());
    guard.record_agreement(c);
    assert!(guard.has_consensus());
}

/// Verifies pass markers are anchored to the first non-empty line (spec P5).
/// A trailing or mid-prose [PASS] no longer counts: bare substring matching
/// was finding F6, the bug class behind botcore's isNoReply incident.
#[test]
fn test_pass_detection() {
    let guard = LoopGuard::new(5, 30);
    assert!(guard.is_pass("[PASS]"));
    assert!(guard.is_pass("[PASS] nothing to add"));
    assert!(!guard.is_pass("I don't have anything to add. [PASS]"));
    assert!(!guard.is_pass("I think we should pass on this feature"));
    assert!(!guard.is_pass("should I emit [PASS] here?"));
}

/// Verifies agreement markers count only at a line start (spec P5), so an
/// agent discussing the protocol does not accidentally vote.
#[test]
fn test_agree_detection() {
    let guard = LoopGuard::new(5, 30);
    assert!(guard.contains_agreement("[AGREE] That sounds right."));
    assert!(guard.contains_agreement("Solid plan overall.\n[AGREE] shipping it."));
    assert!(!guard.contains_agreement("I think so too [AGREE]"));
    assert!(!guard.contains_agreement("I don't agree with that"));
    assert!(!guard.contains_agreement("should I emit [AGREE] here?"));
}
