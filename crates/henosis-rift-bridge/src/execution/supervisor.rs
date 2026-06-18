//! Execution supervisor: spawn execute, rate-limit progress, finalize.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::execution::RoomNotifier;
use crate::executor::{
    AgentExecutor, Capability, ExecutionResult, ExecutionSandbox, ProgressUpdate, TaskContext,
};

/// Minimum interval between progress posts to the room.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Everything the supervisor needs to run one approved task.
pub struct SupervisedTask {
    /// Executor for the proposing agent.
    pub executor: Arc<dyn AgentExecutor>,
    /// Chiasm task id.
    pub task_id: String,
    /// Approved task description.
    pub description: String,
    /// Branch-isolated sandbox.
    pub sandbox: ExecutionSandbox,
    /// Granted capabilities.
    pub granted_capabilities: Vec<Capability>,
}

/// Drives a single `execute()` session: pumps progress to the room with rate
/// limiting and returns the final result.
pub struct ExecutionSupervisor {
    /// Room notifier for progress and result posts.
    notifier: Arc<dyn RoomNotifier>,
}

/// Supervision of execution sessions.
impl ExecutionSupervisor {
    /// Build a supervisor that posts through the given notifier.
    pub fn new(notifier: Arc<dyn RoomNotifier>) -> Self {
        Self { notifier }
    }

    /// Run a supervised task to completion and return its result.
    pub async fn run(&self, task: SupervisedTask) -> ExecutionResult {
        let SupervisedTask {
            executor,
            task_id,
            description,
            sandbox,
            granted_capabilities,
        } = task;

        let max_runtime = sandbox.max_runtime_secs;
        let task_ctx = TaskContext {
            task_id: task_id.clone(),
            description,
            sandbox,
            granted_capabilities,
            prior_context: None,
        };

        let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(64);
        let exec_handle = tokio::spawn(async move { executor.execute(task_ctx, tx).await });

        // Optional wall-clock deadline bounding the WHOLE session, including a
        // hung executor that never sends progress and never returns.
        let deadline = (max_runtime > 0).then(|| Instant::now() + Duration::from_secs(max_runtime));

        // Tracks an unrecoverable failure signalled mid-execution, which is
        // authoritative even if execute() later returns Success.
        let mut failure_signal: Option<String> = None;

        // Pump progress with rate limiting; the channel closes when execute returns.
        let mut last_post: Option<Instant> = None;
        loop {
            let received = match deadline {
                Some(d) => match tokio::time::timeout_at(d, rx.recv()).await {
                    Ok(value) => value,
                    Err(_) => {
                        // Deadline reached. Abort the executor (dropping the
                        // receiver also signals abort via the closed channel),
                        // report, and finalize as a timeout failure.
                        exec_handle.abort();
                        let _ = self
                            .notifier
                            .notify(&format!("[EXEC #{task_id}] timed out after {max_runtime}s"))
                            .await;
                        let result = ExecutionResult::Failed {
                            reason: format!("timed out after {max_runtime}s"),
                            partial_work: true,
                        };
                        self.post_result(&task_id, &result).await;
                        return result;
                    }
                },
                None => rx.recv().await,
            };

            match received {
                Some(ProgressUpdate::Message(m)) => self.maybe_post(&mut last_post, &m).await,
                Some(ProgressUpdate::ToolStarted { tool_name }) => {
                    self.maybe_post(&mut last_post, &format!("running {tool_name}"))
                        .await
                }
                Some(ProgressUpdate::ToolCompleted {
                    tool_name,
                    is_error,
                }) => {
                    let status = if is_error { "failed" } else { "ok" };
                    self.maybe_post(&mut last_post, &format!("{tool_name}: {status}"))
                        .await
                }
                Some(ProgressUpdate::Done) => {}
                Some(ProgressUpdate::Failed(reason)) => {
                    let _ = self
                        .notifier
                        .notify(&format!("[EXEC #{task_id}] failed: {reason}"))
                        .await;
                    failure_signal = Some(reason);
                }
                // Channel closed: execute() has returned, so the join is immediate.
                None => break,
            }
        }

        let result = match exec_handle.await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => ExecutionResult::Failed {
                reason: format!("executor error: {e}"),
                partial_work: false,
            },
            Err(e) => ExecutionResult::Failed {
                reason: format!("execution task panicked: {e}"),
                partial_work: false,
            },
        };

        // A mid-execution unrecoverable-failure signal overrides a Success return.
        let result = match (result, failure_signal) {
            (ExecutionResult::Success { .. }, Some(reason)) => ExecutionResult::Failed {
                reason: format!("executor signalled failure: {reason}"),
                partial_work: true,
            },
            (other, _) => other,
        };

        self.post_result(&task_id, &result).await;
        result
    }

    /// Post a progress line if the rate-limit interval has elapsed.
    async fn maybe_post(&self, last_post: &mut Option<Instant>, line: &str) {
        let now = Instant::now();
        let due = match last_post {
            Some(prev) => now.duration_since(*prev) >= PROGRESS_MIN_INTERVAL,
            None => true,
        };
        if due {
            *last_post = Some(now);
            let _ = self.notifier.notify(&format!("[EXEC] {line}")).await;
        }
    }

    /// Post the terminal result to the room.
    async fn post_result(&self, task_id: &str, result: &ExecutionResult) {
        let msg = match result {
            ExecutionResult::Success {
                summary,
                commit_hash,
                ..
            } => {
                let commit = commit_hash.as_deref().unwrap_or("no commit");
                format!("[EXEC #{task_id}] success: {summary} ({commit})")
            }
            ExecutionResult::Failed {
                reason,
                partial_work,
            } => {
                format!("[EXEC #{task_id}] failed: {reason} (partial_work={partial_work})")
            }
        };
        let _ = self.notifier.notify(&msg).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecutionSupervisor, SupervisedTask};
    use crate::execution::RoomNotifier;
    use crate::executor::{
        AgentExecutor, AgentResponse, Capability, DiscussionContext, ExecutionResult,
        ExecutionSandbox, HealthStatus, ProgressUpdate, TaskContext,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    /// Fake executor that emits two progress messages then succeeds.
    struct FakeExecutor;

    #[async_trait]
    impl AgentExecutor for FakeExecutor {
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
            }
        }
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        async fn execute(
            &self,
            _task: TaskContext,
            progress_tx: mpsc::Sender<ProgressUpdate>,
        ) -> Result<ExecutionResult> {
            let _ = progress_tx
                .send(ProgressUpdate::Message("step one".into()))
                .await;
            let _ = progress_tx
                .send(ProgressUpdate::Message("step two".into()))
                .await;
            let _ = progress_tx.send(ProgressUpdate::Done).await;
            Ok(ExecutionResult::Success {
                summary: "did the thing".into(),
                commit_hash: Some("abc123".into()),
                evidence: None,
            })
        }
        async fn health_check(&self) -> Result<HealthStatus> {
            Ok(HealthStatus::Ready)
        }
    }

    /// Notifier that records every posted message.
    struct RecordingNotifier {
        /// Captured messages.
        posts: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl RoomNotifier for RecordingNotifier {
        async fn notify(&self, content: &str) -> Result<(), crate::error::BridgeError> {
            self.posts.lock().unwrap().push(content.to_string());
            Ok(())
        }
    }

    /// Verifies a successful run posts a final summary and reports completion.
    #[tokio::test]
    async fn test_supervisor_runs_and_finalizes_success() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
            posts: posts.clone(),
        });
        let executor: Arc<dyn AgentExecutor> = Arc::new(FakeExecutor);

        let supervisor = ExecutionSupervisor::new(notifier);
        let task = SupervisedTask {
            executor,
            task_id: "1".into(),
            description: "do work".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
        };

        let result = supervisor.run(task).await;

        match result {
            ExecutionResult::Success { summary, .. } => assert_eq!(summary, "did the thing"),
            ExecutionResult::Failed { .. } => panic!("should succeed"),
        }

        let captured = posts.lock().unwrap();
        // At least the final summary post is present.
        assert!(captured.iter().any(|p| p.contains("did the thing")));
    }
}
