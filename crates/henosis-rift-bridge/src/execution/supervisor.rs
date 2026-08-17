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
use crate::leadership::{wait_for_revocation, LeadershipGuard};

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
    /// Monotonic gate shared with the managed room runtime.
    leadership: Arc<LeadershipGuard>,
}

/// Supervision of execution sessions.
impl ExecutionSupervisor {
    /// Build a supervisor that posts through the given notifier.
    pub fn new(notifier: Arc<dyn RoomNotifier>) -> Self {
        Self {
            notifier,
            leadership: Arc::new(LeadershipGuard::unmanaged()),
        }
    }

    /// Replace the standalone guard with the managed room's shared guard.
    pub fn with_leadership_guard(mut self, leadership: Arc<LeadershipGuard>) -> Self {
        self.leadership = leadership;
        self
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
            if self.leadership.require_current().await.is_err() {
                return leadership_revoked_result(false);
            }
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

            if self.leadership.is_revoked() {
                return result;
            }
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
            if self.leadership.require_current().await.is_err() {
                return leadership_revoked_result(true);
            }
            let next = attempt + 1;
            let notified = self
                .notify_if_current(&format!(
                    "[EXEC #{task_id}] attempt {attempt} left partial work; retrying ({next}/{MAX_ATTEMPTS})"
                ))
                .await;
            if !notified && self.leadership.is_revoked() {
                return leadership_revoked_result(true);
            }
            attempt = next;
        };

        if self.leadership.require_current().await.is_err() {
            return leadership_revoked_result(true);
        }
        if !self.post_result(&task_id, &result).await && self.leadership.is_revoked() {
            return leadership_revoked_result(true);
        }
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
        let leadership_revoked = self.leadership.subscribe();
        if *leadership_revoked.borrow() {
            return leadership_revoked_result(false);
        }
        if self.leadership.require_current().await.is_err() {
            return leadership_revoked_result(false);
        }
        if *leadership_revoked.borrow() {
            return leadership_revoked_result(false);
        }
        let max_runtime = sandbox.max_runtime_secs;
        let task_ctx = TaskContext {
            task_id: task_id.to_string(),
            description,
            sandbox,
            granted_capabilities,
            prior_context,
        };

        let (tx, mut rx) = mpsc::channel::<ProgressUpdate>(64);
        let mut exec_handle = tokio::spawn(async move { executor.execute(task_ctx, tx).await });
        let leadership_revoked = wait_for_revocation(leadership_revoked);
        tokio::pin!(leadership_revoked);

        // Optional wall-clock deadline bounding the WHOLE attempt, including a
        // hung executor that never sends progress and never returns.
        let deadline = (max_runtime > 0).then(|| Instant::now() + Duration::from_secs(max_runtime));

        // Tracks an unrecoverable failure signalled mid-execution, which is
        // authoritative even if execute() later returns Success.
        let mut failure_signal: Option<String> = None;

        // Pump progress with rate limiting; the channel closes when execute returns.
        let mut last_post: Option<Instant> = None;
        loop {
            let receive_progress = async {
                match deadline {
                    Some(d) => tokio::time::timeout_at(d, rx.recv()).await,
                    None => Ok(rx.recv().await),
                }
            };
            let received = tokio::select! {
                biased;
                _ = &mut leadership_revoked => {
                    exec_handle.abort();
                    let _ = (&mut exec_handle).await;
                    return leadership_revoked_result(true);
                }
                value = receive_progress => value,
            };
            let received = match received {
                Ok(value) => value,
                Err(_) => {
                    // Deadline reached. Abort and join the local executor task.
                    // Arbitrary external child side effects that already began
                    // are outside Tokio cancellation and may still persist.
                    exec_handle.abort();
                    let _ = (&mut exec_handle).await;
                    if self.leadership.is_revoked() {
                        return leadership_revoked_result(true);
                    }
                    if !self
                        .notify_if_current(&format!(
                            "[EXEC #{task_id}] timed out after {max_runtime}s"
                        ))
                        .await
                        && self.leadership.is_revoked()
                    {
                        return leadership_revoked_result(true);
                    }
                    return ExecutionResult::Failed {
                        reason: format!("timed out after {max_runtime}s"),
                        partial_work: true,
                    };
                }
            };

            if self.leadership.is_revoked() {
                exec_handle.abort();
                let _ = (&mut exec_handle).await;
                return leadership_revoked_result(true);
            }

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
                    self.notify_if_current(&format!("[EXEC #{task_id}] failed: {reason}"))
                        .await;
                    failure_signal = Some(reason);
                }
                // Channel closed: execute() has returned, so the join is immediate.
                None => break,
            }
        }

        if self.leadership.is_revoked() {
            exec_handle.abort();
            let _ = (&mut exec_handle).await;
            return leadership_revoked_result(true);
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
        if self.leadership.is_revoked() {
            return;
        }
        let now = Instant::now();
        let due = match last_post {
            Some(prev) => now.duration_since(*prev) >= PROGRESS_MIN_INTERVAL,
            None => true,
        };
        if due {
            *last_post = Some(now);
            self.notify_if_current(&format!("[EXEC] {line}")).await;
        }
    }

    /// Post the terminal result to the room.
    async fn post_result(&self, task_id: &str, result: &ExecutionResult) -> bool {
        if self.leadership.is_revoked() {
            return false;
        }
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
        self.notify_if_current(&msg).await
    }

    /// Post one notice while racing the shared revocation signal.
    async fn notify_if_current(&self, message: &str) -> bool {
        let revoked = self.leadership.subscribe();
        if *revoked.borrow() {
            return false;
        }
        let revoked = wait_for_revocation(revoked);
        tokio::pin!(revoked);
        tokio::select! {
            biased;
            _ = &mut revoked => false,
            result = self.notifier.notify(message) => {
                if result.is_err() && self.leadership.is_managed() {
                    self.leadership.revoke();
                }
                result.is_ok() && !self.leadership.is_revoked()
            }
        }
    }
}

/// Build the terminal local result used when managed leadership is revoked.
fn leadership_revoked_result(partial_work: bool) -> ExecutionResult {
    ExecutionResult::Failed {
        reason: "managed-room leadership revoked; local executor cancellation cannot roll back external side effects that already began".to_string(),
        partial_work,
    }
}

#[cfg(test)]
/// Covers successful, retrying, timed-out, and leadership-revoked supervision.
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::{mpsc, Semaphore};

    use crate::leadership::LeadershipGuard;

    /// Fake executor that emits two progress messages then succeeds. Records the
    /// `prior_context` it receives so tests can assert the supervisor threads it.
    struct FakeExecutor {
        /// Captures the prior_context the executor was handed.
        seen_prior: Arc<Mutex<Option<String>>>,
    }

    /// Supplies a successful executor while capturing resume context.
    #[async_trait]
    impl AgentExecutor for FakeExecutor {
        /// Declares the fake's required shell capability.
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        /// Returns the fake executor's inert sandbox policy.
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }
        /// Produces no conversational response in supervisor tests.
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        /// Records context, emits progress, and returns success.
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
        /// Reports the fake executor ready for work.
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

    /// Fails once with partial work and then succeeds for retry coverage.
    #[async_trait]
    impl AgentExecutor for PartialThenSuccessExecutor {
        /// Declares the fake's required shell capability.
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        /// Returns the fake executor's inert sandbox policy.
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }
        /// Produces no conversational response in supervisor tests.
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        /// Records context and returns the attempt-specific result.
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
        /// Reports the fake executor ready for work.
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

    /// Always returns a clean terminal failure for no-retry coverage.
    #[async_trait]
    impl AgentExecutor for CleanFailExecutor {
        /// Declares the fake's required shell capability.
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        /// Returns the fake executor's inert sandbox policy.
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }
        /// Produces no conversational response in supervisor tests.
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        /// Counts the attempt and returns a clean failure.
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
        /// Reports the fake executor ready for work.
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

    /// Drop marker proving an aborted executor future was joined and destroyed.
    struct AbortDropProbe {
        /// Shared observation flipped when the executor future is dropped.
        dropped: Arc<AtomicBool>,
    }

    /// Marks completion of local cancellation for the active executor future.
    impl Drop for AbortDropProbe {
        /// Records that the cancelled executor future was destroyed.
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// Executor that enters one attempt and then waits forever until aborted.
    struct CancellableExecutor {
        /// Number of attempts that reached executor code.
        attempts: Arc<AtomicUsize>,
        /// Permit released once the executor future is active.
        entered: Arc<Semaphore>,
        /// Observation flipped only when the active future is dropped.
        dropped: Arc<AtomicBool>,
    }

    /// Provides a cancellation-observable execution fake.
    #[async_trait]
    impl AgentExecutor for CancellableExecutor {
        /// Declares the fake's required shell capability.
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }

        /// Returns the fake executor's inert sandbox policy.
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }

        /// Produces no conversational response in supervisor tests.
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }

        /// Signals entry and remains pending until supervision cancels it.
        async fn execute(
            &self,
            _task: TaskContext,
            _progress_tx: mpsc::Sender<ProgressUpdate>,
        ) -> Result<ExecutionResult> {
            let _drop_probe = AbortDropProbe {
                dropped: self.dropped.clone(),
            };
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.entered.add_permits(1);
            std::future::pending::<Result<ExecutionResult>>().await
        }

        /// Reports the fake executor ready for work.
        async fn health_check(&self) -> Result<HealthStatus> {
            Ok(HealthStatus::Ready)
        }
    }

    /// Times out once and then succeeds for timeout retry coverage.
    #[async_trait]
    impl AgentExecutor for TimeoutThenSuccessExecutor {
        /// Declares the fake's required shell capability.
        fn required_capabilities(&self) -> Vec<Capability> {
            vec![Capability::new(Capability::BASH)]
        }
        /// Returns the fake executor's inert sandbox policy.
        fn sandbox(&self) -> ExecutionSandbox {
            ExecutionSandbox {
                branch: "agent/a/task-1".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            }
        }
        /// Produces no conversational response in supervisor tests.
        async fn discuss(&self, _c: DiscussionContext) -> Result<Option<AgentResponse>> {
            Ok(None)
        }
        /// Hangs on attempt one and succeeds on attempt two.
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
        /// Reports the fake executor ready for work.
        async fn health_check(&self) -> Result<HealthStatus> {
            Ok(HealthStatus::Ready)
        }
    }

    /// Notifier that records every posted message.
    struct RecordingNotifier {
        /// Captured messages.
        posts: Arc<Mutex<Vec<String>>>,
    }

    /// Captures every room notice in memory for assertions.
    #[async_trait]
    impl RoomNotifier for RecordingNotifier {
        /// Appends one notice to the shared capture list.
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

    /// Revocation before attempt one prevents executor entry and all room notices.
    #[tokio::test]
    async fn revoked_before_start_never_enters_executor() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
            posts: posts.clone(),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let executor: Arc<dyn AgentExecutor> = Arc::new(CleanFailExecutor {
            attempts: attempts.clone(),
        });
        let leadership = Arc::new(LeadershipGuard::unmanaged());
        leadership.revoke();
        let supervisor = ExecutionSupervisor::new(notifier).with_leadership_guard(leadership);
        let task = SupervisedTask {
            executor,
            task_id: "10".into(),
            description: "must not start".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-10".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: None,
        };

        let result = supervisor.run(task).await;

        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        assert!(posts.lock().unwrap().is_empty());
        assert!(matches!(
            result,
            ExecutionResult::Failed {
                partial_work: false,
                ..
            }
        ));
    }

    /// Active revocation aborts and joins the local task without retry or notices.
    #[tokio::test]
    async fn active_revocation_aborts_joins_and_never_retries() {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let notifier: Arc<dyn RoomNotifier> = Arc::new(RecordingNotifier {
            posts: posts.clone(),
        });
        let attempts = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Semaphore::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let executor: Arc<dyn AgentExecutor> = Arc::new(CancellableExecutor {
            attempts: attempts.clone(),
            entered: entered.clone(),
            dropped: dropped.clone(),
        });
        let leadership = Arc::new(LeadershipGuard::unmanaged());
        let supervisor =
            Arc::new(ExecutionSupervisor::new(notifier).with_leadership_guard(leadership.clone()));
        let task = SupervisedTask {
            executor,
            task_id: "11".into(),
            description: "cancel on takeover".into(),
            sandbox: ExecutionSandbox {
                branch: "agent/a/task-11".into(),
                working_dir: PathBuf::from("/tmp"),
                max_runtime_secs: 0,
                cargo_target_dir: None,
            },
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            prior_context: None,
        };
        let run = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.run(task).await }
        });
        let entered_permit = tokio::time::timeout(Duration::from_secs(2), entered.acquire())
            .await
            .expect("executor should enter")
            .expect("entry semaphore open");
        entered_permit.forget();

        leadership.revoke();
        let result = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("revoked run should terminate")
            .expect("supervisor task should join");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(dropped.load(Ordering::SeqCst));
        assert!(posts.lock().unwrap().is_empty());
        match result {
            ExecutionResult::Failed {
                reason,
                partial_work,
            } => {
                assert!(partial_work);
                assert!(reason.contains("cannot roll back external side effects"));
            }
            ExecutionResult::Success { .. } => panic!("revoked execution cannot succeed"),
        }
    }
}
