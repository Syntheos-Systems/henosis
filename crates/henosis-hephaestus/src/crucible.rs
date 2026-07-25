//! In-process Crucible adapter for Hephaestus task specifications and verification gates.
//!
//! The adapter owns no process path or temporary files. It dispatches directly into the
//! shared Crucible service, which moves blocking SQLite and command work onto Tokio's
//! blocking pool.

use crucible::{Crucible, Output, Tool};
use serde_json::{Value, json};
use std::path::Path;
use tracing::{debug, warn};

/// Optional in-process Crucible integration used by the Hephaestus task lifecycle.
pub struct CrucibleClient {
    /// The service is absent only when the operator explicitly disables Crucible.
    service: Option<Crucible>,
}

/// Creates task specifications and runs completion verification through Crucible.
impl CrucibleClient {
    /// Open the configured Crucible database when integration is enabled.
    pub fn open(enabled: bool, database_path: &Path) -> Result<Self, String> {
        let service = if enabled {
            Some(
                Crucible::open(database_path)
                    .map_err(|error| format!("could not open Crucible database: {error}"))?,
            )
        } else {
            None
        };
        Ok(Self { service })
    }

    /// Register a Hephaestus task in Crucible and return its specification id.
    pub async fn spec_task(&self, task_id: &str, title: &str, description: &str) -> Option<String> {
        let input = json!({
            "task_description": format!("hephaestus task {task_id}: {title}: {description}"),
            "task_type": "feature",
            "acceptance_criteria": [
                "agent loop reaches end_turn or stop without exceeding max_tool_turns",
                "all tool calls return a tool_result before the next turn"
            ],
            "edge_cases": [
                "provider authorization fails during a turn",
                "ask_human pauses until the task is explicitly resumed",
                "process restart resumes from the latest checkpoint without duplicate threads"
            ],
            "files_to_touch": ["crates/henosis-hephaestus/src/tasks.rs"],
            "interface_contract": format!("POST /tasks task_id={task_id}"),
        });
        self.run(Tool::SpecTask, input)
            .await
            .and_then(|output| output.id)
    }

    /// Run a completion command through Crucible and require its verification to pass.
    pub async fn verify(&self, command: &str) -> bool {
        self.run(
            Tool::Verify,
            json!({
                "command": command,
                "timeout_secs": 300
            }),
        )
        .await
        .is_some_and(|output| output.success)
    }

    /// Dispatch one tool through the enabled service and normalize failures for callers.
    async fn run(&self, tool: Tool, input: Value) -> Option<Output> {
        let Some(service) = &self.service else {
            return None;
        };
        let output = service.run(tool, input).await;
        if output.success {
            debug!(tool = ?tool, "Crucible gate completed");
            Some(output)
        } else {
            warn!(tool = ?tool, error = %output.message, "Crucible gate failed");
            Some(output)
        }
    }
}

/// In-process integration tests for the Hephaestus Crucible adapter.
#[cfg(test)]
mod tests {
    use super::CrucibleClient;

    /// Task registration persists a Crucible specification without a subprocess.
    #[tokio::test]
    async fn registers_specification_in_process() {
        let directory = tempfile::tempdir().expect("tempdir");
        let client =
            CrucibleClient::open(true, &directory.path().join("crucible.db")).expect("open");

        let id = client
            .spec_task("task-1", "Test task", "Exercise Crucible")
            .await;

        assert!(
            id.as_deref()
                .is_some_and(|value| value.starts_with("spec_"))
        );
    }

    /// Disabled integration performs no filesystem work and returns no specification.
    #[tokio::test]
    async fn disabled_integration_is_inert() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = directory.path().join("missing/crucible.db");
        let client = CrucibleClient::open(false, &database).expect("disabled");

        assert!(
            client
                .spec_task("task-1", "Test", "Disabled")
                .await
                .is_none()
        );
        assert!(!database.exists());
    }
}
