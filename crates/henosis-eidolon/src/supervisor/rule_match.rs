//! Regex rule matching over a session-JSONL entry's text surfaces.
//!
//! Ported (copy-and-own) from Kleos `eidolon-supervisor/src/checks/rule_match.rs`, with one
//! deviation: patterns arrive PRE-COMPILED ([`CompiledRule`]) instead of being re-compiled per
//! entry per rule, and an invalid pattern is a construction error rather than a silently
//! skipped check (fail-closed: a rule that cannot run must not pretend it ran).

use super::{CheckType, CompiledRule, Violation};

/// Run every `RuleMatch` rule against the entry's combined text surfaces.
pub fn check(entry: &serde_json::Value, rules: &[CompiledRule]) -> Vec<Violation> {
    let mut violations = Vec::new();
    let text = extract_check_text(entry);
    if text.is_empty() {
        return violations;
    }
    for compiled in rules {
        if !matches!(compiled.rule.check_type, CheckType::RuleMatch) {
            continue;
        }
        if let Some(re) = &compiled.regex {
            if re.is_match(&text) {
                violations.push(Violation {
                    rule_id: compiled.rule.id.clone(),
                    severity: compiled.rule.severity,
                    message: compiled.rule.message.clone(),
                    context: super::truncate(&text, 200),
                    session_id: None,
                });
            }
        }
    }
    violations
}

/// Collect the entry's checkable text: tool command/content, assistant text, commit messages.
fn extract_check_text(entry: &serde_json::Value) -> String {
    let obj = match entry.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    let mut parts = Vec::new();
    // Tool input: Bash commands and file contents.
    if let Some(input) = obj.get("tool_input").or(obj.get("input")) {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            parts.push(cmd.to_string());
        }
        if let Some(content) = input.get("content").and_then(|v| v.as_str()) {
            parts.push(content.to_string());
        }
    }
    // Assistant text output.
    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
        parts.push(text.to_string());
    }
    if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
        parts.push(content.to_string());
    }
    // Commit messages in git operations.
    if let Some(msg) = obj.get("message").and_then(|v| v.as_str()) {
        parts.push(msg.to_string());
    }
    parts.join("\n")
}
