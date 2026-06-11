//! Template narration and the pluggable LLM-narrator seam.
//!
//! The template table is ported from the Kleos narrator (itself a port of the Ghost-Frame
//! `narrator.ts`). Template narration is pure and synchronous; anything smarter goes through
//! the [`Narrator`] trait, an async seam the server can fill with a Synapse/Foundry-backed
//! implementation at wiring time (Phase 4) without changing this crate -- the same
//! evolve-without-breaking pattern as the dispatcher's `OutputFilter` slot (G1).

use async_trait::async_trait;

use crate::error::BrocaError;

/// Action-type to template lookup. Each template uses `{{key}}` placeholders substituted from
/// the action payload at narration time. Missing payload keys are left as the literal
/// `{{key}}` text so gaps are visible rather than silently suppressed (Kleos parity).
const TEMPLATES: &[(&str, &str)] = &[
    // ---- Chiasm / tasks ----
    (
        "task.created",
        "{{agent}} started a new task: \"{{title}}\" in {{project}}",
    ),
    ("task.updated", "\"{{title}}\" status is now {{status}}"),
    ("task.completed", "\"{{title}}\" was completed by {{agent}}"),
    ("task.blocked", "\"{{title}}\" is blocked: {{reason}}"),
    (
        "task.blocked_on_human",
        "\"{{title}}\" is waiting for human approval: {{summary}}",
    ),
    (
        "task.feedback",
        "Human feedback on \"{{title}}\": \"{{feedback}}\"",
    ),
    ("task.output", "Output submitted for \"{{title}}\""),
    ("task.plan", "A plan was generated for \"{{title}}\""),
    (
        "task.unblocked",
        "\"{{title}}\" was unblocked: all dependencies completed",
    ),
    // ---- Loom / workflows ----
    (
        "workflow.run.created",
        "{{agent}} started the \"{{workflow}}\" workflow",
    ),
    (
        "workflow.run.completed",
        "The \"{{workflow}}\" workflow finished successfully",
    ),
    (
        "workflow.run.failed",
        "The \"{{workflow}}\" workflow failed on step \"{{failed_step}}\": {{error}}",
    ),
    (
        "workflow.run.cancelled",
        "The \"{{workflow}}\" workflow was cancelled",
    ),
    (
        "workflow.step.started",
        "Step \"{{step}}\" started in the \"{{workflow}}\" workflow",
    ),
    (
        "workflow.step.completed",
        "Step \"{{step}}\" finished in the \"{{workflow}}\" workflow",
    ),
    (
        "workflow.step.failed",
        "Step \"{{step}}\" failed in the \"{{workflow}}\" workflow: {{error}}",
    ),
    // ---- Soma / agents ----
    (
        "agent.registered",
        "{{name}} came online as a {{agent_type}}",
    ),
    ("agent.deregistered", "{{name}} went offline"),
    ("agent.online", "{{agent}} is online"),
    ("agent.offline", "{{agent}} went offline"),
    ("agent.heartbeat", "{{agent}} checked in"),
    ("agent.error", "{{agent}} reported an error: {{error}}"),
    // ---- Memory ----
    ("memory.stored", "{{source}} stored a memory ({{category}})"),
    (
        "memory.searched",
        "{{agent}} searched memory for \"{{query}}\"",
    ),
    ("memory.linked", "Two memories were linked together"),
    ("memory.forgotten", "A memory was removed"),
    // ---- Thymus / evaluations ----
    (
        "evaluation.completed",
        "{{agent}}'s work on \"{{subject}}\" was evaluated using the {{rubric}} rubric",
    ),
    (
        "metric.recorded",
        "{{agent}} recorded {{metric}}: {{value}}",
    ),
    // ---- System ----
    ("system.started", "{{service}} started up"),
    ("system.stopped", "{{service}} shut down"),
    ("deploy.started", "Deployment started for {{service}}"),
    ("deploy.succeeded", "{{service}} deployed successfully"),
    (
        "deploy.failed",
        "Deployment failed for {{service}}: {{error}}",
    ),
    ("deploy.rolled_back", "{{service}} was rolled back"),
    ("alert.triggered", "Alert triggered: {{message}}"),
];

/// Render a template-based narrative for the given action type and payload.
///
/// Returns `None` if no template is registered for `action`. `{{key}}` placeholders are
/// replaced from the payload's top-level keys (strings verbatim, other values via their JSON
/// text); missing keys remain as the literal `{{key}}` so callers can see which fields were
/// absent.
pub fn narrate_from_template(action: &str, payload: &serde_json::Value) -> Option<String> {
    let template = TEMPLATES
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, t)| *t)?;
    let mut out = template.to_string();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            let needle = format!("{{{{{k}}}}}");
            let replacement = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out = out.replace(&needle, &replacement);
        }
    }
    Some(out)
}

/// The pluggable narrator: turns an action that has no template into a short English sentence.
///
/// The Phase 1 store ships with no narrator (template-or-nothing). A Synapse/Foundry-backed
/// implementation plugs in at server wiring time (Phase 4); failures are decoration failures
/// ([`BrocaError::Narration`]) and never affect the logged action itself.
#[async_trait]
pub trait Narrator: Send + Sync {
    /// Produce a one-sentence human-readable narrative for `action` and its `payload`.
    async fn narrate(
        &self,
        action: &str,
        payload: &serde_json::Value,
    ) -> Result<String, BrocaError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// String payload values substitute verbatim; non-strings use their JSON text.
    #[test]
    fn template_substitutes_payload_keys() {
        let narrative = narrate_from_template(
            "task.created",
            &serde_json::json!({"agent": "claude", "title": "ship it", "project": "henosis"}),
        )
        .expect("template exists");
        assert_eq!(
            narrative,
            "claude started a new task: \"ship it\" in henosis"
        );
    }

    /// Missing payload keys stay visible as the literal placeholder.
    #[test]
    fn missing_keys_remain_visible() {
        let narrative =
            narrate_from_template("task.updated", &serde_json::json!({})).expect("template exists");
        assert!(narrative.contains("{{title}}"));
        assert!(narrative.contains("{{status}}"));
    }

    /// Unknown action types have no template.
    #[test]
    fn unknown_action_is_none() {
        assert!(narrate_from_template("nope.never", &serde_json::json!({})).is_none());
    }
}
