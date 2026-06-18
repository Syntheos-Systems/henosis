use henosis_rift_bridge::turn_manager::TurnManager;
use henosis_rift_bridge::types::AgentId;
use uuid::Uuid;

/// Verifies one agent cannot post twice in a row when another agent is available.
#[test]
fn test_interleaving_blocks_consecutive_posts() {
    let mut tm = TurnManager::new(2000, 8000);
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
    let mut tm = TurnManager::new(2000, 8000);
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
    let mut tm = TurnManager::new(2000, 8000);
    let agent_a = AgentId(Uuid::new_v4());

    tm.record_post(agent_a);
    tm.record_human_post();
    assert!(
        tm.can_post_next(agent_a),
        "agent should be allowed after human posted"
    );
}
