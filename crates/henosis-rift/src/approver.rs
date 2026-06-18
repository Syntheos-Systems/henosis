//! [`RegistryApprover`]: an out-of-band approval channel backed by a pending map.
//!
//! The gate blocks on [`crate::gate::Approver::await_decision`], which registers
//! a one-shot keyed by the approval id and waits with a deadline. A human's
//! response arrives later through a different path (Rift -> the server),
//! carrying the same approval id, and the server calls [`RegistryApprover::resolve`]
//! to wake the waiter. No response before the deadline yields
//! [`ApprovalDecision::TimedOut`] (which the gate maps to a denial -- fail-closed).

use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use tokio::sync::oneshot;

use crate::gate::{ApprovalDecision, ApprovalRequest, Approver};

/// An approver that parks each request on a one-shot until an out-of-band
/// [`RegistryApprover::resolve`] call delivers the human's decision or the
/// deadline elapses.
pub struct RegistryApprover {
    /// Approval id -> the waiter waiting for its decision.
    pending: DashMap<String, oneshot::Sender<ApprovalDecision>>,
    /// How long to wait for a human before timing out (denying).
    timeout: Duration,
}

impl RegistryApprover {
    /// Build an approver that denies (times out) after `timeout`.
    pub fn new(timeout: Duration) -> Self {
        Self {
            pending: DashMap::new(),
            timeout,
        }
    }

    /// Deliver a human's decision for `approval_id`, waking its waiter.
    ///
    /// Returns `true` if a matching pending request was found and notified,
    /// `false` if there was no such request (already resolved, timed out, or
    /// unknown id).
    pub fn resolve(&self, approval_id: &str, decision: ApprovalDecision) -> bool {
        match self.pending.remove(approval_id) {
            Some((_, tx)) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Number of approvals currently awaiting a decision.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[async_trait]
impl Approver for RegistryApprover {
    /// Register `request` and block until [`RegistryApprover::resolve`] delivers
    /// a decision or the deadline elapses. The pending entry is always removed
    /// before returning, so a timed-out id cannot later be resolved.
    async fn await_decision(&self, request: &ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        self.pending.insert(request.approval_id.clone(), tx);

        match tokio::time::timeout(self.timeout, rx).await {
            // A decision was delivered via resolve().
            Ok(Ok(decision)) => decision,
            // The sender was dropped without a decision (defensive); fail-closed.
            Ok(Err(_)) => {
                self.pending.remove(&request.approval_id);
                ApprovalDecision::TimedOut
            }
            // Deadline elapsed before any decision; remove the stale waiter.
            Err(_) => {
                self.pending.remove(&request.approval_id);
                ApprovalDecision::TimedOut
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Build an approval request with a fixed id.
    fn req(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_id: id.to_owned(),
            prompt: "ok?".to_owned(),
            tool: "synapse".to_owned(),
            action: "deploy".to_owned(),
        }
    }

    /// A decision delivered before the deadline wakes the waiter with it.
    #[tokio::test]
    async fn resolve_before_timeout_delivers_decision() {
        let approver = Arc::new(RegistryApprover::new(Duration::from_secs(5)));
        let a = approver.clone();
        let waiter = tokio::spawn(async move { a.await_decision(&req("abc")).await });

        // Wait until the waiter has registered, then resolve it.
        while approver.pending_count() == 0 {
            tokio::task::yield_now().await;
        }
        assert!(approver.resolve("abc", ApprovalDecision::Approved));

        assert_eq!(waiter.await.unwrap(), ApprovalDecision::Approved);
        assert_eq!(approver.pending_count(), 0);
    }

    /// A denial is delivered verbatim.
    #[tokio::test]
    async fn resolve_delivers_denial() {
        let approver = Arc::new(RegistryApprover::new(Duration::from_secs(5)));
        let a = approver.clone();
        let waiter = tokio::spawn(async move { a.await_decision(&req("d1")).await });
        while approver.pending_count() == 0 {
            tokio::task::yield_now().await;
        }
        approver.resolve("d1", ApprovalDecision::Denied("nope".to_owned()));
        assert_eq!(
            waiter.await.unwrap(),
            ApprovalDecision::Denied("nope".to_owned())
        );
    }

    /// No decision before the deadline times out (and cleans up the waiter).
    #[tokio::test]
    async fn no_decision_times_out() {
        let approver = RegistryApprover::new(Duration::from_millis(40));
        let decision = approver.await_decision(&req("slow")).await;
        assert_eq!(decision, ApprovalDecision::TimedOut);
        assert_eq!(approver.pending_count(), 0);
    }

    /// Resolving an unknown id reports no waiter found.
    #[tokio::test]
    async fn resolve_unknown_id_is_false() {
        let approver = RegistryApprover::new(Duration::from_secs(5));
        assert!(!approver.resolve("ghost", ApprovalDecision::Approved));
    }
}
