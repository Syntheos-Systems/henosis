//! Retry-loop detection: the same command issued several times in a row.
//!
//! A bounded history of recent commands triggers when its tail holds `RETRY_THRESHOLD`
//! identical entries.

use std::collections::VecDeque;

use super::{CheckType, CompiledRule, Violation};

/// How many recent commands the tracker remembers.
const MAX_HISTORY: usize = 10;

/// How many identical consecutive commands constitute a retry loop.
const RETRY_THRESHOLD: usize = 3;

/// Sliding window of the most recent tool commands.
pub struct RetryTracker {
    /// The recent commands, oldest first, capped at [`MAX_HISTORY`].
    recent_commands: VecDeque<String>,
}

/// Implements retry-loop tracking and rule evaluation.
impl RetryTracker {
    /// An empty tracker.
    pub fn new() -> Self {
        Self {
            recent_commands: VecDeque::with_capacity(MAX_HISTORY),
        }
    }

    /// Record the entry's command (if any) and fire the `RetryLoop` rule when the most recent
    /// [`RETRY_THRESHOLD`] commands are identical.
    pub fn check(&mut self, entry: &serde_json::Value, rules: &[CompiledRule]) -> Vec<Violation> {
        let cmd = match extract_command(entry) {
            Some(c) => c,
            None => return Vec::new(),
        };
        self.recent_commands.push_back(cmd.clone());
        if self.recent_commands.len() > MAX_HISTORY {
            self.recent_commands.pop_front();
        }
        let consecutive = self
            .recent_commands
            .iter()
            .rev()
            .take_while(|c| *c == &cmd)
            .count();
        if consecutive >= RETRY_THRESHOLD {
            let rule = rules
                .iter()
                .find(|r| matches!(r.rule.check_type, CheckType::RetryLoop));
            if let Some(compiled) = rule {
                return vec![Violation {
                    rule_id: compiled.rule.id.clone(),
                    severity: compiled.rule.severity,
                    message: format!(
                        "{} ({} repeats of: {})",
                        compiled.rule.message,
                        consecutive,
                        super::truncate(&cmd, 80)
                    ),
                    context: cmd,
                    session_id: None,
                }];
            }
        }
        Vec::new()
    }
}

/// An empty tracker is the default.
impl Default for RetryTracker {
    /// Builds an empty retry tracker.
    fn default() -> Self {
        Self::new()
    }
}

/// Pull the tool command string out of a JSONL entry.
fn extract_command(entry: &serde_json::Value) -> Option<String> {
    let obj = entry.as_object()?;
    let input = obj.get("tool_input").or(obj.get("input"))?;
    input
        .get("command")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}
