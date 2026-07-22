//! Pre-spawn checks the bridge runs before dispatching an approved task.
//!
//! The `AgentExecutor` trait documents that the bridge consults `health_check`
//! and `sandbox` "before spawning". These two pure helpers encode those
//! decisions so `Room::execute_approved` stays thin and the logic is unit
//! testable without standing up a live room.
//!
//! - [`health_preflight`] turns a `HealthStatus` into a spawn decision
//!   (closes known-incomplete ledger row 17: a caller now invokes
//!   `health_check`).
//! - [`apply_runtime_policy`] clamps a worktree sandbox's wall-clock limit to
//!   the executor's self-declared ceiling (closes row 16: the bridge now reads
//!   `executor.sandbox()` as runtime policy).

use crate::executor::{ExecutionSandbox, HealthStatus};

/// Outcome of the pre-spawn health check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preflight {
    /// The executor is ready; spawn may proceed.
    Proceed,
    /// The executor is degraded but usable; proceed and log the reason.
    ProceedDegraded(String),
    /// The executor is unavailable; do not spawn. Carries the reason to record
    /// on the blocked task.
    Block(String),
}

/// Decide whether to spawn given an executor's reported health.
///
/// `Ready` proceeds, `Degraded` proceeds with a logged reason, and
/// `Unavailable` blocks. The bridge calls this before creating the sandbox so a
/// dead runtime never gets a worktree.
pub fn health_preflight(status: HealthStatus) -> Preflight {
    match status {
        HealthStatus::Ready => Preflight::Proceed,
        HealthStatus::Degraded(reason) => Preflight::ProceedDegraded(reason),
        HealthStatus::Unavailable(reason) => Preflight::Block(reason),
    }
}

/// Clamp a bridge-created sandbox's runtime to the executor's declared ceiling.
///
/// The bridge owns branch/path derivation; the executor owns its runtime
/// policy. The effective limit is the smaller of the operator-configured limit
/// (already on `sandbox`) and the executor's declared `max_runtime_secs`, so an
/// executor may self-limit but never exceed the operator ceiling. A policy of
/// `0` (no executor-declared limit) leaves the operator value untouched.
pub fn apply_runtime_policy(
    mut sandbox: ExecutionSandbox,
    policy: &ExecutionSandbox,
) -> ExecutionSandbox {
    if policy.max_runtime_secs > 0 {
        sandbox.max_runtime_secs = if sandbox.max_runtime_secs == 0 {
            // Operator set no limit; adopt the executor's declared ceiling.
            policy.max_runtime_secs
        } else {
            sandbox.max_runtime_secs.min(policy.max_runtime_secs)
        };
    }
    sandbox
}

/// Unit tests for execution readiness and concurrency policy.
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a sandbox carrying just the runtime limit under test.
    fn sandbox_with_runtime(secs: u64) -> ExecutionSandbox {
        ExecutionSandbox {
            branch: "agent/test/task-1".into(),
            working_dir: PathBuf::from("/tmp/task-1"),
            max_runtime_secs: secs,
            cargo_target_dir: None,
        }
    }

    /// `Ready` proceeds without any note.
    #[test]
    fn ready_proceeds() {
        assert_eq!(health_preflight(HealthStatus::Ready), Preflight::Proceed);
    }

    /// `Degraded` proceeds but surfaces the reason for logging.
    #[test]
    fn degraded_proceeds_with_reason() {
        let decision = health_preflight(HealthStatus::Degraded("slow provider".into()));
        assert_eq!(decision, Preflight::ProceedDegraded("slow provider".into()));
    }

    /// `Unavailable` blocks and carries the reason to record on the task.
    #[test]
    fn unavailable_blocks_with_reason() {
        let decision = health_preflight(HealthStatus::Unavailable("no model configured".into()));
        assert_eq!(decision, Preflight::Block("no model configured".into()));
    }

    /// A lower executor ceiling clamps the operator-configured limit down.
    #[test]
    fn executor_ceiling_clamps_operator_limit() {
        let sandbox = sandbox_with_runtime(3600);
        let policy = sandbox_with_runtime(600);
        let clamped = apply_runtime_policy(sandbox, &policy);
        assert_eq!(clamped.max_runtime_secs, 600);
    }

    /// A higher executor ceiling never raises the operator limit.
    #[test]
    fn executor_ceiling_never_raises_operator_limit() {
        let sandbox = sandbox_with_runtime(600);
        let policy = sandbox_with_runtime(3600);
        let clamped = apply_runtime_policy(sandbox, &policy);
        assert_eq!(clamped.max_runtime_secs, 600);
    }

    /// A zero policy (no executor-declared limit) leaves the operator value.
    #[test]
    fn zero_policy_keeps_operator_limit() {
        let sandbox = sandbox_with_runtime(600);
        let policy = sandbox_with_runtime(0);
        let clamped = apply_runtime_policy(sandbox, &policy);
        assert_eq!(clamped.max_runtime_secs, 600);
    }

    /// A zero operator limit (unbounded) adopts the executor's declared ceiling.
    #[test]
    fn zero_operator_limit_adopts_executor_ceiling() {
        let sandbox = sandbox_with_runtime(0);
        let policy = sandbox_with_runtime(3600);
        let clamped = apply_runtime_policy(sandbox, &policy);
        assert_eq!(clamped.max_runtime_secs, 3600);
    }

    /// Branch and working dir are bridge-owned: the policy never overrides them.
    #[test]
    fn policy_leaves_branch_and_path_bridge_owned() {
        let sandbox = sandbox_with_runtime(600);
        let mut policy = sandbox_with_runtime(3600);
        policy.branch = "agent/synapse/unset".into();
        policy.working_dir = PathBuf::from("/elsewhere");
        let clamped = apply_runtime_policy(sandbox, &policy);
        assert_eq!(clamped.branch, "agent/test/task-1");
        assert_eq!(clamped.working_dir, PathBuf::from("/tmp/task-1"));
    }
}
