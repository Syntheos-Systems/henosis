//! Process-boundary tests for the Codex CLI executor.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use henosis_rift_bridge::executor::{
    AgentExecutor, DiscussionContext, ExecutionResult, ExecutionSandbox, HealthStatus, TaskContext,
};
use henosis_rift_bridge::executors::CodexExecutor;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Fake Codex process that records its complete boundary and replays fixture output.
struct FakeCodex {
    /// Isolated directory containing the executable and recordings.
    root: PathBuf,
    /// Executable shell fixture passed to the executor.
    binary: PathBuf,
}

/// Builds and inspects one isolated fake Codex process.
impl FakeCodex {
    /// Create an executable fixture with predetermined stdout, stderr, and exit status.
    fn new(stdout: &str, stderr: &str, exit_code: i32) -> Self {
        let root = std::env::temp_dir().join(format!("henosis-codex-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).expect("create fake Codex directory");
        fs::write(root.join("stdout"), stdout).expect("write fake stdout");
        fs::write(root.join("stderr"), stderr).expect("write fake stderr");
        let binary = root.join("codex");
        let script = format!(
            "#!/bin/sh\n\
             : > '{root}/args'\n\
             for arg in \"$@\"; do printf '%s\\n' \"$arg\" >> '{root}/args'; done\n\
             cat > '{root}/stdin'\n\
             if [ \"${{CARGO_TARGET_DIR+x}}\" = x ]; then\n\
               printf '%s' \"$CARGO_TARGET_DIR\" > '{root}/cargo_target_dir'\n\
             else\n\
               printf '%s' '<unset>' > '{root}/cargo_target_dir'\n\
             fi\n\
             cat '{root}/stdout'\n\
             cat '{root}/stderr' >&2\n\
             exit {exit_code}\n",
            root = root.display(),
        );
        fs::write(&binary, script).expect("write fake Codex executable");
        let mut permissions = fs::metadata(&binary)
            .expect("stat fake Codex")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&binary, permissions).expect("make fake Codex executable");
        Self { root, binary }
    }

    /// Return the recorded argument vector in order.
    fn args(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("args"))
            .expect("read recorded arguments")
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Return the exact prompt received on stdin.
    fn stdin(&self) -> String {
        fs::read_to_string(self.root.join("stdin")).expect("read recorded stdin")
    }

    /// Return the recorded Cargo target directory or the unset marker.
    fn cargo_target_dir(&self) -> String {
        fs::read_to_string(self.root.join("cargo_target_dir"))
            .expect("read recorded Cargo target directory")
    }
}

/// Removes only the UUID-scoped fixture directory after each test.
impl Drop for FakeCodex {
    /// Delete the isolated fixture after its process has completed.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Construct the minimal discussion input needed by process tests.
fn discussion_context() -> DiscussionContext {
    DiscussionContext {
        recent_messages: Vec::new(),
        persona_name: None,
        relevant_memories: Vec::new(),
        active_tasks_summary: None,
        channel_id: "general".to_string(),
        system_framing: Some("Answer precisely.".to_string()),
    }
}

/// Construct an approved execution task rooted in the fake workspace.
fn task_context(working_dir: &Path, cargo_target_dir: Option<PathBuf>) -> TaskContext {
    TaskContext {
        task_id: "task-7".to_string(),
        description: "Implement the approved change.".to_string(),
        sandbox: ExecutionSandbox {
            branch: "agent/codex/task-7".to_string(),
            working_dir: working_dir.to_path_buf(),
            max_runtime_secs: 600,
            cargo_target_dir,
        },
        granted_capabilities: Vec::new(),
        prior_context: None,
    }
}

/// Discussion uses exact separate arguments, read-only sandboxing, and piped stdin.
#[tokio::test]
async fn discussion_uses_guarded_argument_vector_and_stdin() {
    let fake = FakeCodex::new(
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Done\"}}\n",
        "",
        0,
    );
    let executor = CodexExecutor::new(
        fake.binary.clone(),
        "gpt-5.6-sol".to_string(),
        Some("medium".to_string()),
    );

    executor
        .discuss(discussion_context())
        .await
        .expect("discussion succeeds");

    assert_eq!(
        fake.args(),
        [
            "exec",
            "--ephemeral",
            "--json",
            "--model",
            "gpt-5.6-sol",
            "--sandbox",
            "read-only",
            "-c",
            "model_reasoning_effort=\"medium\"",
            "-",
        ]
    );
    assert!(fake.stdin().contains("Answer precisely."));
    assert_eq!(fake.cargo_target_dir(), "<unset>");
}

/// Execution adds the approved worktree, workspace-write sandbox, and scoped Cargo cache.
#[tokio::test]
async fn execution_uses_guarded_argument_vector_and_scoped_environment() {
    let fake = FakeCodex::new(
        "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Implemented.\"}}\n",
        "",
        0,
    );
    let executor = CodexExecutor::new(fake.binary.clone(), "gpt-5.6-sol".to_string(), None);
    let target_dir = fake.root.join("cargo-target");
    let (progress_tx, _progress_rx) = mpsc::channel(4);

    let result = executor
        .execute(
            task_context(&fake.root, Some(target_dir.clone())),
            progress_tx,
        )
        .await
        .expect("execution succeeds");

    assert!(matches!(result, ExecutionResult::Success { .. }));
    assert_eq!(
        fake.args(),
        vec![
            "exec".to_string(),
            "--ephemeral".to_string(),
            "--json".to_string(),
            "--model".to_string(),
            "gpt-5.6-sol".to_string(),
            "--cd".to_string(),
            fake.root.display().to_string(),
            "--sandbox".to_string(),
            "workspace-write".to_string(),
            "-".to_string(),
        ]
    );
    assert_eq!(fake.stdin(), "Implement the approved change.");
    assert_eq!(fake.cargo_target_dir(), target_dir.display().to_string());
}

/// JSONL parsing ignores other records and returns only the final completed agent message.
#[tokio::test]
async fn discussion_extracts_final_completed_agent_message() {
    let fake = FakeCodex::new(
        concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread-1\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"First\"}}\n",
            "{\"type\":\"item.started\",\"item\":{\"type\":\"agent_message\",\"text\":\"Ignore me\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\",\"text\":\"Ignore me too\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"Final answer\"}}\n",
        ),
        "",
        0,
    );
    let executor = CodexExecutor::new(fake.binary.clone(), "gpt-5.6-sol".to_string(), None);

    let response = executor
        .discuss(discussion_context())
        .await
        .expect("discussion succeeds")
        .expect("agent responds");

    assert_eq!(response.text, "Final answer");
}

/// A nonzero Codex execution maps to a structured failed result with stderr.
#[tokio::test]
async fn execution_maps_nonzero_exit_to_failure() {
    let fake = FakeCodex::new("", "model unavailable", 17);
    let executor = CodexExecutor::new(fake.binary.clone(), "gpt-5.6-sol".to_string(), None);
    let (progress_tx, _progress_rx) = mpsc::channel(4);

    let result = executor
        .execute(task_context(&fake.root, None), progress_tx)
        .await
        .expect("process failure is a task result");

    assert!(matches!(
        result,
        ExecutionResult::Failed { reason, partial_work: false }
            if reason.contains("17") && reason.contains("model unavailable")
    ));
    assert_eq!(fake.cargo_target_dir(), "<unset>");
}

/// Successful JSONL without a completed agent message is an intentional discussion pass.
#[tokio::test]
async fn discussion_returns_none_for_empty_final_response() {
    let fake = FakeCodex::new(
        "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":1}}\n",
        "",
        0,
    );
    let executor = CodexExecutor::new(fake.binary.clone(), "gpt-5.6-sol".to_string(), None);

    let response = executor
        .discuss(discussion_context())
        .await
        .expect("discussion succeeds");

    assert!(response.is_none());
}

/// Health reporting identifies a configured binary that does not exist.
#[tokio::test]
async fn health_reports_missing_binary() {
    let missing = std::env::temp_dir().join(format!("missing-codex-{}", Uuid::new_v4()));
    let executor = CodexExecutor::new(missing.clone(), "gpt-5.6-sol".to_string(), None);

    let health = executor
        .health_check()
        .await
        .expect("health check succeeds");

    assert_eq!(
        health,
        HealthStatus::Unavailable(format!("codex binary not found at {}", missing.display()))
    );
}
