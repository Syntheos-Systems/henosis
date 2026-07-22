//! Integration tests for turn interleaving and compose-slot geometry.

use henosis_rift_bridge::turn_manager::TurnManager;
use henosis_rift_bridge::types::AgentId;
use uuid::Uuid;

/// Verifies one agent cannot post twice in a row when another agent is available.
#[test]
fn test_interleaving_blocks_consecutive_posts() {
    let mut tm = TurnManager::new(6000, 4000);
    let agent_a = AgentId(Uuid::new_v4());
    let agent_b = AgentId(Uuid::new_v4());

    tm.record_post(agent_a);
    assert!(
        !tm.can_post_next(agent_a),
        "same agent should not post consecutively"
    );
    assert!(
        tm.can_post_next(agent_b),
        "different agent should be allowed"
    );
}

/// Verifies interleaving allows an agent again after another agent posts.
#[test]
fn test_interleaving_resets_after_other_agent() {
    let mut tm = TurnManager::new(6000, 4000);
    let agent_a = AgentId(Uuid::new_v4());
    let agent_b = AgentId(Uuid::new_v4());

    tm.record_post(agent_a);
    tm.record_post(agent_b);
    assert!(
        tm.can_post_next(agent_a),
        "agent_a should be allowed after agent_b posted"
    );
}

/// Verifies a human post clears the interleaving block.
#[test]
fn test_human_message_resets_interleaving() {
    let mut tm = TurnManager::new(6000, 4000);
    let agent_a = AgentId(Uuid::new_v4());

    tm.record_post(agent_a);
    tm.record_human_post();
    assert!(
        tm.can_post_next(agent_a),
        "agent should be allowed after human posted"
    );
}

/// Verifies distinct slot indices never share a compose window because jitter
/// stays inside each agent's own slot.
#[test]
fn test_slots_are_disjoint_across_agents() {
    let tm = TurnManager::new(6000, 4000);
    for slot in 0..4usize {
        for _ in 0..32 {
            let d = tm.slot_delay(slot).as_millis() as u64;
            assert!(d >= slot as u64 * 6000, "delay below slot floor");
            assert!(d <= slot as u64 * 6000 + 4000, "delay above slot ceiling");
        }
    }
}
