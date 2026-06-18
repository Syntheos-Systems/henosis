use henosis_rift_bridge::context::{build_discussion_context, to_cli_prompt};

/// Verifies context assembly copies the expected channel, framing, memory, and task fields.
#[test]
fn test_build_context_populates_fields() {
    let ctx = build_discussion_context(
        "You are an architect.",
        "Architect",
        "general",
        vec![
            ("Alice", "We need to redesign the auth flow."),
            ("Bob", "I agree, the current one is fragile."),
        ],
        vec!["Alice", "Bob", "Architect"],
        vec![
            "[memory:17 source=human] previous decision: keep JWT auth".into(),
            "[memory:18 source=agent] room consensus favored smaller context windows".into(),
        ],
        Some("#44 [builder] Patch room context (active)".into()),
        None,
        None,
    );

    assert_eq!(ctx.channel_id, "general");
    assert!(ctx.system_framing.as_ref().unwrap().contains("Architect"));
    assert_eq!(ctx.relevant_memories.len(), 2);
    assert_eq!(
        ctx.active_tasks_summary.as_deref(),
        Some("#44 [builder] Patch room context (active)")
    );
    assert_eq!(ctx.recent_messages.len(), 2);
}

/// Verifies CLI prompt rendering includes system, messages, memory, and task sections.
#[test]
fn test_cli_prompt_includes_system_messages_memories_and_tasks() {
    let ctx = build_discussion_context(
        "You are an architect.",
        "Architect",
        "general",
        vec![("Alice", "We need to redesign the auth flow.")],
        vec!["Alice", "Architect"],
        vec!["[memory:17 source=human] previous decision: keep JWT auth".into()],
        Some("#44 [builder] Patch room context (active)".into()),
        None,
        None,
    );

    let prompt = to_cli_prompt(&ctx);
    assert!(prompt.contains("You are an architect."));
    assert!(prompt.contains("Alice: We need to redesign the auth flow."));
    assert!(prompt.contains("Relevant memories"));
    assert!(prompt.contains("Active tasks"));
}

/// Verifies an empty conversation still renders a useful CLI prompt.
#[test]
fn test_empty_messages_still_produces_prompt() {
    let ctx = build_discussion_context(
        "You are a reviewer.",
        "Reviewer",
        "general",
        Vec::<(&str, &str)>::new(),
        vec!["Reviewer"],
        Vec::<String>::new(),
        None,
        None,
        None,
    );

    let prompt = to_cli_prompt(&ctx);
    assert!(prompt.contains("You are a reviewer."));
    assert!(prompt.contains("No recent messages"));
}

/// Verifies an assigned persona and growth notes are injected into the context.
#[test]
fn test_persona_and_growth_are_injected() {
    let ctx = build_discussion_context(
        "You are an engineer.",
        "Engineer",
        "general",
        Vec::<(&str, &str)>::new(),
        vec!["Engineer"],
        Vec::<String>::new(),
        None,
        Some("skeptic".into()),
        Some("Prior note: the cache layer is fragile.".into()),
    );

    assert_eq!(ctx.persona_name.as_deref(), Some("skeptic"));
    let framing = ctx.system_framing.as_ref().unwrap();
    assert!(framing.contains("skeptic"), "persona should be in framing");
    assert!(
        framing.contains("the cache layer is fragile"),
        "growth notes should be in framing"
    );
}
