//! Approval registry for pending execution proposals.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::execution::{PendingProposal, ProposalId};
use crate::executor::Capability;

/// One registered proposal plus its creation instant for TTL.
struct Entry {
    /// The pending proposal awaiting approval.
    proposal: PendingProposal,
    /// When the proposal was registered.
    created: Instant,
}

/// Thread-safe registry of pending approvals, shared by the room and control
/// server. Cheap to clone (internally `Arc`).
#[derive(Clone)]
pub struct ApprovalRegistry {
    /// Shared inner state.
    inner: Arc<Mutex<HashMap<ProposalId, Entry>>>,
    /// Monotonic id source.
    next_id: Arc<AtomicU64>,
    /// Time-to-live for a pending proposal.
    ttl: Duration,
}

/// Registration, approval, rejection, and expiry of pending proposals.
impl ApprovalRegistry {
    /// Create an empty registry with the given approval TTL in seconds.
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Register a new proposal and return its assigned id.
    pub fn register(
        &self,
        agent: String,
        task_id: String,
        scope_summary: String,
        granted_capabilities: Vec<Capability>,
        workspace: String,
    ) -> ProposalId {
        let id = ProposalId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let proposal = PendingProposal {
            id,
            agent,
            task_id,
            scope_summary,
            granted_capabilities,
            workspace,
        };
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .insert(
                id,
                Entry {
                    proposal,
                    created: Instant::now(),
                },
            );
        id
    }

    /// Insert a pre-built proposal directly. Test helper.
    #[cfg(test)]
    pub fn insert_for_test(&self, proposal: PendingProposal) -> ProposalId {
        let id = proposal.id;
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .insert(
                id,
                Entry {
                    proposal,
                    created: Instant::now(),
                },
            );
        id
    }

    /// Approve a proposal, removing and returning it if it was pending.
    pub fn approve(&self, id: ProposalId) -> Option<PendingProposal> {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .remove(&id)
            .map(|e| e.proposal)
    }

    /// Reject a proposal, removing it. Returns true if it was present.
    pub fn reject(&self, id: ProposalId) -> bool {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .remove(&id)
            .is_some()
    }

    /// Remove and return all proposals whose TTL has elapsed.
    pub fn sweep_expired(&self) -> Vec<PendingProposal> {
        let now = Instant::now();
        let mut guard = self.inner.lock().expect("approval registry mutex poisoned");
        let expired: Vec<ProposalId> = guard
            .iter()
            .filter(|(_, e)| now.duration_since(e.created) >= self.ttl)
            .map(|(id, _)| *id)
            .collect();
        expired
            .into_iter()
            .filter_map(|id| guard.remove(&id).map(|e| e.proposal))
            .collect()
    }

    /// List all currently pending proposals (clones).
    pub fn list(&self) -> Vec<PendingProposal> {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .values()
            .map(|e| e.proposal.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalRegistry;
    use crate::execution::{PendingProposal, ProposalId};
    use crate::executor::Capability;

    /// Builds a pending proposal with the given id for registry tests.
    fn proposal(id: u64) -> PendingProposal {
        PendingProposal {
            id: ProposalId(id),
            agent: "architect".into(),
            task_id: id.to_string(),
            scope_summary: "work".into(),
            granted_capabilities: vec![Capability::new(Capability::BASH)],
            workspace: "rift".into(),
        }
    }

    /// Verifies register assigns increasing ids and the proposal is listable.
    #[test]
    fn test_register_assigns_ids_and_lists() {
        let reg = ApprovalRegistry::new(1800);
        let id1 = reg.register(
            "architect".into(),
            "1".into(),
            "work".into(),
            vec![],
            "rift".into(),
        );
        let id2 = reg.register(
            "architect".into(),
            "2".into(),
            "work".into(),
            vec![],
            "rift".into(),
        );
        assert_ne!(id1, id2);
        assert_eq!(reg.list().len(), 2);
    }

    /// Verifies approve removes and returns the proposal once.
    #[test]
    fn test_approve_returns_and_removes() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(7));
        let taken = reg.approve(ProposalId(7));
        assert!(taken.is_some());
        assert!(reg.approve(ProposalId(7)).is_none());
        assert!(reg.list().is_empty());
    }

    /// Verifies reject removes the proposal and returns true once.
    #[test]
    fn test_reject_removes() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(9));
        assert!(reg.reject(ProposalId(9)));
        assert!(!reg.reject(ProposalId(9)));
    }

    /// Verifies expired proposals are swept and returned.
    #[test]
    fn test_sweep_expired_removes_old_entries() {
        let reg = ApprovalRegistry::new(0); // ttl 0 -> everything is immediately expired
        let _ = reg.insert_for_test(proposal(3));
        let expired = reg.sweep_expired();
        assert_eq!(expired.len(), 1);
        assert!(reg.list().is_empty());
    }
}
