//! Discussion context assembly.
//!
//! Builds synapse-core's `DiscussionContext` from bridge-internal state.
//! Also provides `to_cli_prompt()` for the ClaudeCodeExecutor fallback.

use crate::executor::{ConversationMessage, DiscussionContext};

/// Build a synapse-core `DiscussionContext` from bridge-level data.
///
/// The bridge knows about agents, channels, and team members. This function
/// maps that into the structured context that executors consume.
#[allow(clippy::too_many_arguments)]
pub fn build_discussion_context(
    system_prompt: &str,
    agent_name: &str,
    channel_name: &str,
    recent_messages: Vec<(&str, &str)>,
    team_members: Vec<&str>,
    relevant_memories: Vec<String>,
    active_tasks_summary: Option<String>,
    persona_name: Option<String>,
    growth: Option<String>,
) -> DiscussionContext {
    let team_list = team_members
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    // A persona, when assigned, is stated up front so the agent stays in character.
    let persona_framing = match &persona_name {
        Some(p) => format!("\nYou are playing the persona: {p}. Stay in character."),
        None => String::new(),
    };

    // The agent's growth file (its own running notes for this project), if any.
    let growth_framing = match growth.as_deref() {
        Some(g) if !g.trim().is_empty() => format!("\n\n--- Your notes so far ---\n{g}\n---"),
        _ => String::new(),
    };

    let system_framing = format!(
        "{}{}\n\nYou are {} in the #{} channel.\n\
         Other team members: {}\n\
         Respond concisely. If you have nothing to add, say exactly: [PASS]\n\
         If you agree with the current consensus, include [AGREE] in your message.{}",
        system_prompt, persona_framing, agent_name, channel_name, team_list, growth_framing
    );

    let messages = recent_messages
        .into_iter()
        .map(|(author, text)| ConversationMessage {
            author: author.to_string(),
            text: text.to_string(),
            timestamp_secs: 0,
        })
        .collect();

    DiscussionContext {
        recent_messages: messages,
        persona_name,
        relevant_memories,
        active_tasks_summary,
        channel_id: channel_name.to_string(),
        system_framing: Some(system_framing),
    }
}

/// Render a `DiscussionContext` into a flat prompt string for `claude -p`.
///
/// Used by the ClaudeCodeExecutor which needs a single prompt string.
/// SynapseExecutor has its own internal formatting and does not use this.
pub fn to_cli_prompt(ctx: &DiscussionContext) -> String {
    let mut parts = Vec::new();

    if let Some(framing) = &ctx.system_framing {
        parts.push(framing.clone());
    }

    if ctx.recent_messages.is_empty() {
        parts.push("\n--- Recent conversation ---\nNo recent messages.\n---".to_string());
    } else {
        let mut history = String::from("\n--- Recent conversation ---\n");
        for msg in &ctx.recent_messages {
            history.push_str(&format!("{}: {}\n", msg.author, msg.text));
        }
        history.push_str("---");
        parts.push(history);
    }

    if !ctx.relevant_memories.is_empty() {
        parts.push(format!(
            "\n--- Relevant memories ---\n{}\n---",
            ctx.relevant_memories.join("\n")
        ));
    }

    if let Some(tasks) = &ctx.active_tasks_summary {
        parts.push(format!("\n--- Active tasks ---\n{}\n---", tasks));
    }

    parts.join("\n")
}
