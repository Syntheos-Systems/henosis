//! Execution supervisor: spawn execute, rate-limit progress, finalize.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::execution::resume;
use crate::execution::RoomNotifier;
use crate::executor::{
    AgentExecutor, Capability, ExecutionResult, ExecutionSandbox, ProgressUpdate, TaskContext,
};

/// Minimum interval between progress posts to the room.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum number of execution attempts: the initial attempt plus one retry
/// when an attempt fails leaving partial work on the worktree.
const MAX_ATTEMPTS: u32 = 2;

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
    /// Optional prior context: a summary of partial work from a previous attempt,
    /// threaded into the executor's `TaskContext` (crash-recovery resume path). The
    /// first-attempt caller passes `None`; the resume path will populate it once it
    /// exists. The supervisor must not discard it.
    pub prior_context: Option<String>,
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
    ///
    /// A single partial-work failure is retried once against the SAME worktree.
    /// The supervisor captures the branch base before the first attempt; on a
    /// `Failed { partial_work: true }` result under the attempt budget it builds a
    /// resume summary (the failure reason plus the commits the attempt left
    /// behind) and re-dispatches the task with that summary as `prior_context`.
    /// The terminal result is posted to the room exactly once, after the loop.
    /// A clean failure or a success is terminal and never retries.
    pub async fn run(&self, task: SupervisedTask) -> ExecutionResult {
        let SupervisedTask {
            executor,
            task_id,
            description,
            sandbox,
            granted_capabilities,
            prior_context,
        } = task;

        // Branch base before attempt 1: the worktree HEAD is the branch base with
        // no commits yet, so anything a failed attempt commits is reachable as
        // `base..HEAD` for the resume summary. `None` (non-repo / git error)
        // degrades to an empty commit list and never blocks the retry.
        let base = resume::git_head(&sandbox.working_dir).await;

        // `prior_context` for the next attempt: the preset value on attempt 1
        // (the first-dispatch caller passes `None`), then a built resume summary.
        let mut prior_context = prior_context;
        let mut attempt: u32 = 1;
        let result = loop {
            let result = self
                .run_attempt(
                    executor.clone(),
                    &task_id,
                    description.clone(),
                    sandbox.clone(),
                    granted_capabilities.clone(),
                    prior_context.take(),
                )
                .await;

            if !resume::should_retry(&result, attempt, MAX_ATTEMPTS) {
                break result;
            }

            // Partial work was left behind and budget remains: build the resume
            // summary from the commits this attempt added, tell the room, retry.
            let reason = match &result {
                ExecutionResult::Failed { reason, .. } => reason.clone(),
                ExecutionResult::Success { .. } => String::new(),
            };
            let commits = match &base {
                Some(base) => resume::collect_partial_commits(&sandbox.working_dir, base).await,
                None => Vec::new(),
            };
            prior_context = Some(resume::format_resume_context(&reason, &commits));
            let next = attempt + 1;
            let _ = self
                .notifier
                .notify(&format!(
                    "[EXEC #{task_id}] attempt {attempt} left partial work; retrying ({next}/{MAX_ATTEMPTS})"
                ))
                .await;
            attempt = next;
        };

        self.post_result(&task_id, &result).await;
        result
    }

    /// Run a single execution attempt and return its result WITHOUT posting it.
    ///
    /// Spawns `execute`, pumps rate-limited progress to the room, enforces the
    /// optional wall-clock deadline, and applies the mid-execution failure-signal
    /// override. The caller (`run`) decides retry and posts the terminal result
    /// once, so this method never calls `post_result`.
    async fn run_attempt(
        &self,
        executor: Arc<dyn AgentExecutor>,
        task_id: &str,
        description: String,
        sandbox: ExecutionSandbox,
        granted_capabilities: Vec<Capability>,
        prior_context: Option<String>,
    ) -> ExecutionResult {
        let max_runtime = sandbox.max_runtime_secs;
        let task_ctx = TaskContext {
            task_id: task_id.to_string(),
            description,
            sandbox,
            granted_capabilities,
            prior_context,
        };

        let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(64);
        let exec_handle = tokio::spawn(async move { executor.execute(task_ctx, tx).await });

        // Optional wall-clock deadline bounding the WHOLE attempt, including a
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
                        // report, and return a timeout failure that is eligible
                        // for one retry (partial work may exist on the branch).
                        exec_handle.abort();
                        let _ = self
                            .notifier
                            .notify(&format!("[EXEC #{task_id}] timed out after {max_runtime}s"))
                            .await;
                        return ExecutionResult::Failed {
                            reason: format!("timed out after {max_runtime}s"),
                            partial_work: true,
                        };
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
        match (result, failure_signal) {
            (ExecutionResult::Success { .. }, Some(reason)) => ExecutionResult::Failed {
                reason: format!("executor signalled failure: {reason}"),
                partial_work: true,
            },
            (other, _) => other,
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    /// Fake executor that emits two progress messages then succeeds. Records the
    /// `prior_context` it receives so tests can assert the supervisor threads it.
    struct FakeExecutor {
        /// Captures the prior_context the executor was handed.
        seen_prior: Arc<Mutex<Option<String>>>,
    }

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
                cargo_target_dir: None,
            }
        }
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        async fn execute(
            &self,
            task: TaskContext,
            progress_tx: mpsc::Sender<ProgressUpdate>,
        ) -> Result<ExecutionResult> {
            *self.seen_prior.lock().unwrap() = task.prior_context.clone();
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

    /// Fake executor that fails leaving partial work on its first attempt and
    /// succeeds on the second. Records the `prior_context` seen at each attempt
    /// so tests can assert the supervisor builds and threads a resume summary.
    struct PartialThenSuccessExecutor {
        /// Number of times `execute` has been entered.
        attempts: Arc<AtomicUsize>,
        /// The `prior_context` captured at each attempt, in order.
        priors: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl AgentExecutor for PartialThenSuccessExecutor {
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        async fn execute(
            &self,
            task: TaskContext,
            progress_tx: mpsc::Sender<ProgressUpdate>,
        ) -> Result<ExecutionResult> {
            self.priors.lock().unwrap().push(task.prior_context.clone());
            let n = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            let _ = progress_tx.send(ProgressUpdate::Done).await;
            if n == 1 {
                Ok(ExecutionResult::Failed {
                    reason: "left partial work".into(),
                    partial_work: true,
                })
            } else {
                Ok(ExecutionResult::Success {
                    summary: "finished on retry".into(),
                    commit_hash: Some("def456".into()),
                    evidence: None,
                })
            }
        }
        async fn health_check(&self) -> Result<HealthStatus> {
            Ok(HealthStatus::Ready)
        }
    }

    /// Fake executor that always fails WITHOUT leaving partial work. Used to
    /// prove a clean failure is terminal and never retries.
    struct CleanFailExecutor {
        /// Number of times `execute` has been entered.
        attempts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl AgentExecutor for CleanFailExecutor {
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
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
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let _ = progress_tx.send(ProgressUpdate::Done).await;
            Ok(ExecutionResult::Failed {
                reason: "config invalid".into(),
                partial_work: false,
            })
        }
        async fn health_check(&self) -> Result<HealthStatus> {
            Ok(HealthStatus::Ready)
        }
    }

    /// Fake executor that hangs past any deadline on its first attempt (so the
    /// supervisor's wall-clock timeout fires and aborts it), then succeeds
    /// immediately on the second. Records the `prior_context` seen at each
    /// attempt. Proves a timeout -- the primary production trigger for
    /// `partial_work: true` -- is retried exactly once.
    struct TimeoutThenSuccessExecutor {
        /// Number of times `execute` has been entered.
        attempts: Arc<AtomicUsize>,
        /// The `prior_context` captured at each attempt, in order.
        priors: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[async_trait]
    impl AgentExecutor for TimeoutThenSuccessExecutor {
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        async fn execute(
            &self,
            task: TaskContext,
            _progress_tx: mpsc::Sender<ProgressUpdate>,
        ) -> Result<ExecutionResult> {
            self.priors.lock().unwrap().push(task.prior_context.clone());
            let n = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                // Hang well past the test deadline; the supervisor aborts this
                // task at the deadline, so the value returned here is never used.
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                return Ok(ExecutionResult::Failed {
                    reason: "not aborted".into(),
                    partial_work: false,
                });
            }
            Ok(ExecutionResult::Success {
                summary: "succeeded after timeout".into(),
                commit_hash: None,
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
        let seen_prior = Arc::new(Mutex::new(None));
        let executor: Arc<dyn AgentExecutor> = Arc::new(FakeExecutor {
            seen_prior: seen_prior.clone(),
        });

        let supervisor = ExecutionSupervisor::new(notifier);
        let task = SupervisedTask {
            executor,
            task_id: "1".into(),
            description: "do work".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: Some("partial work from attempt 1".into()),
        };

        let result = supervisor.run(task).await;

        match result {
            ExecutionResult::Success { summary, .. } => assert_eq!(summary, "did the thing"),
            ExecutionResult::Failed { .. } => panic!("should succeed"),
        }

        // The supervisor must thread prior_context into the executor, not drop it.
        assert_eq!(
            seen_prior.lock().unwrap().as_deref(),
            Some("partial work from attempt 1")
        );

        let captured = posts.lock().unwrap();
        // At least the final summary post is present.
        assert!(captured.iter().any(|p| p.contains("did the thing")));
    }

    /// A partial-work failure on attempt 1 triggers exactly one retry that
    /// receives a resume summary, and the retry's success is the final result.
    #[tokio::test]
    async fn test_supervisor_retries_once_on_partial_then_succeeds() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
            posts: posts.clone(),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let priors = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn AgentExecutor> = Arc::new(PartialThenSuccessExecutor {
            attempts: attempts.clone(),
            priors: priors.clone(),
        });

        let supervisor = ExecutionSupervisor::new(notifier);
        let task = SupervisedTask {
            executor,
            task_id: "7".into(),
            description: "do work".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: None,
        };

        let result = supervisor.run(task).await;

        // Exactly two attempts ran: the partial failure retried once.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        // The retry's success is the supervisor's final result.
        match result {
            ExecutionResult::Success { summary, .. } => assert_eq!(summary, "finished on retry"),
            ExecutionResult::Failed { .. } => panic!("retry should have succeeded"),
        }
        // First attempt saw the preset prior_context (None); the retry saw a
        // built resume summary naming the prior failure.
        let seen = priors.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], None);
        assert!(seen[1]
            .as_deref()
            .unwrap()
            .contains("A previous attempt failed"));
        // The room was told a retry happened.
        let captured = posts.lock().unwrap();
        assert!(captured.iter().any(|p| p.contains("retrying (2/2)")));
    }

    /// A clean failure (no partial work) is terminal: the supervisor runs the
    /// task once and never retries.
    #[tokio::test]
    async fn test_supervisor_does_not_retry_on_clean_failure() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
            posts: posts.clone(),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let executor: Arc<dyn AgentExecutor> = Arc::new(CleanFailExecutor {
            attempts: attempts.clone(),
        });

        let supervisor = ExecutionSupervisor::new(notifier);
        let task = SupervisedTask {
            executor,
            task_id: "8".into(),
            description: "do work".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: None,
        };

        let result = supervisor.run(task).await;

        // A clean failure runs exactly once.
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        match result {
            ExecutionResult::Failed { partial_work, .. } => assert!(!partial_work),
            ExecutionResult::Success { .. } => panic!("should have failed"),
        }
    }

    /// A wall-clock timeout on attempt 1 leaves partial work and is retried
    /// once; the retry receives the timeout reason as prior context and
    /// succeeds, and the terminal result is posted exactly once.
    #[tokio::test]
    async fn test_supervisor_retries_once_after_timeout() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
            posts: posts.clone(),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let priors = Arc::new(Mutex::new(Vec::new()));
        let executor: Arc<dyn AgentExecutor> = Arc::new(TimeoutThenSuccessExecutor {
            attempts: attempts.clone(),
            priors: priors.clone(),
        });

        let supervisor = ExecutionSupervisor::new(notifier);
        let task = SupervisedTask {
            executor,
            task_id: "9".into(),
            description: "do work".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 1,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: None,
        };

        let result = supervisor.run(task).await;

        // The timeout retried once and the retry succeeded.
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        match result {
            ExecutionResult::Success { summary, .. } => {
                assert_eq!(summary, "succeeded after timeout")
            }
            ExecutionResult::Failed { .. } => panic!("retry after timeout should have succeeded"),
        }
        // The retry's prior context names the timeout as the failure reason.
        let seen = priors.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].as_deref().unwrap().contains("timed out after 1s"));
        // The room saw both the timeout notice and the retry notice, and exactly
        // one terminal success result (post_result fires once after the loop).
        let captured = posts.lock().unwrap();
        assert!(captured.iter().any(|p| p.contains("timed out after 1s")));
        assert!(captured.iter().any(|p| p.contains("retrying (2/2)")));
        assert_eq!(
            captured
                .iter()
                .filter(|p| p.contains("success: succeeded after timeout"))
                .count(),
            1
        );
    }
}
