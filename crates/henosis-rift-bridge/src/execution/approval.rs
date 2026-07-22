//! Approval registry for pending execution proposals.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::execution::{PendingProposal, ProposalId};
use crate::executor::Capability;

/// Lifecycle state of a registered proposal.
///
/// The `Approved` state exists so a proposal approved while the bridge is
/// paused stays IN the registry instead of being carried in a private queue.
/// Held approvals used to vanish from `/control/approvals` entirely: an
/// operator could neither see nor cancel them until someone unpaused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalState {
    /// Awaiting a human decision.
    Pending,
    /// Approved by a human, not yet dispatched (the bridge is paused).
    Approved,
}

/// One registered proposal plus its creation instant for TTL.
struct Entry {
    /// The pending proposal awaiting approval.
    proposal: PendingProposal,
    /// When the proposal was registered.
    created: Instant,
    /// Where the proposal sits in its lifecycle.
    state: ProposalState,
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
                    state: ProposalState::Pending,
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
                    state: ProposalState::Pending,
                },
            );
        id
    }

    /// Approve a proposal IN PLACE, returning a clone for dispatch.
    ///
    /// The entry deliberately stays in the registry: if the bridge is paused
    /// the dispatch is held, and a held approval must remain listable and
    /// rejectable. The caller removes it with `complete` once dispatched.
    /// Returns None when the id is unknown or already approved, so a double
    /// approve cannot queue a second execution of the same task.
    pub fn approve(&self, id: ProposalId) -> Option<PendingProposal> {
        let mut guard = self.inner.lock().expect("approval registry mutex poisoned");
        let entry = guard.get_mut(&id)?;
        if entry.state != ProposalState::Pending {
            return None;
        }
        entry.state = ProposalState::Approved;
        Some(entry.proposal.clone())
    }

    /// Approve and remove in one step, for callers that dispatch immediately.
    ///
    /// The in-room `!approve` path only runs while unpaused, so its proposal
    /// never needs the held state and should not linger in the registry.
    pub fn approve_and_take(&self, id: ProposalId) -> Option<PendingProposal> {
        let mut guard = self.inner.lock().expect("approval registry mutex poisoned");
        match guard.get(&id) {
            Some(e) if e.state == ProposalState::Pending => guard.remove(&id).map(|e| e.proposal),
            _ => None,
        }
    }

    /// Remove an entry after its dispatch has been handed off.
    ///
    /// Returns false when the entry is already gone -- rejected during a hold,
    /// or claimed by `take_approved` on unpause. Callers dispatch ONLY on a
    /// true return, which is what makes dispatch exactly-once when unpause and
    /// the approval channel race.
    pub fn complete(&self, id: ProposalId) -> bool {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .remove(&id)
            .is_some()
    }

    /// Remove and return every approved-but-undispatched proposal.
    ///
    /// Called on unpause to flush approvals held during the pause.
    pub fn take_approved(&self) -> Vec<PendingProposal> {
        let mut guard = self.inner.lock().expect("approval registry mutex poisoned");
        let approved: Vec<ProposalId> = guard
            .iter()
            .filter(|(_, e)| e.state == ProposalState::Approved)
            .map(|(id, _)| *id)
            .collect();
        approved
            .into_iter()
            .filter_map(|id| guard.remove(&id).map(|e| e.proposal))
            .collect()
    }

    /// Reject a proposal, removing it. Returns true if it was present.
    ///
    /// Works in any state, so an operator can cancel an approval that is being
    /// held through a pause.
    pub fn reject(&self, id: ProposalId) -> bool {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .remove(&id)
            .is_some()
    }

    /// Remove and return all PENDING proposals whose TTL has elapsed.
    ///
    /// Approved entries are exempt. The TTL exists so proposals nobody acted on
    /// do not pile up; an approved one has been acted on, and expiring it
    /// because the operator happened to pause would discard a human decision.
    /// A held approval cannot hide either way -- it stays listed and rejectable.
    pub fn sweep_expired(&self) -> Vec<PendingProposal> {
        let now = Instant::now();
        let mut guard = self.inner.lock().expect("approval registry mutex poisoned");
        let expired: Vec<ProposalId> = guard
            .iter()
            .filter(|(_, e)| {
                e.state == ProposalState::Pending && now.duration_since(e.created) >= self.ttl
            })
            .map(|(id, _)| *id)
            .collect();
        expired
            .into_iter()
            .filter_map(|id| guard.remove(&id).map(|e| e.proposal))
            .collect()
    }

    /// List all currently registered proposals (clones).
    pub fn list(&self) -> Vec<PendingProposal> {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .values()
            .map(|e| e.proposal.clone())
            .collect()
    }

    /// List all registered proposals with their lifecycle state.
    ///
    /// Backs the control API so an operator can tell a proposal still awaiting
    /// a decision from one already approved and waiting on unpause.
    pub fn list_with_state(&self) -> Vec<(PendingProposal, ProposalState)> {
        self.inner
            .lock()
            .expect("approval registry mutex poisoned")
            .values()
            .map(|e| (e.proposal.clone(), e.state))
            .collect()
    }
}

/// What the approvals drain task should do with a proposal it just received.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainAction {
    /// Paused: leave the entry in the registry, visible and rejectable.
    Hold,
    /// Hand off to the dispatcher.
    Dispatch,
    /// Already handled elsewhere; dispatching would run the task twice.
    Skip,
}

/// Decide the drain task's action for a received approval.
///
/// Extracted from the drain loop so the exactly-once rule is testable: the
/// unpause flush (`take_approved`) and the approval channel can both carry the
/// same proposal, and only the one that still owns the registry entry may
/// dispatch it. A rejection during a hold lands here as `Skip` too.
pub fn decide_drain_action(
    paused: bool,
    registry: &ApprovalRegistry,
    id: ProposalId,
) -> DrainAction {
    if paused {
        DrainAction::Hold
    } else if registry.complete(id) {
        DrainAction::Dispatch
    } else {
        DrainAction::Skip
    }
}

/// Covers the proposal lifecycle, the held-approval state, and the
/// exactly-once rule the drain task depends on.
#[cfg(test)]
mod tests {
    use super::{decide_drain_action, ApprovalRegistry, DrainAction, ProposalState};
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

    /// Verifies the full control-server cycle: approve once, then complete.
    ///
    /// Approve no longer removes the entry -- it holds it so a pause cannot
    /// make the proposal invisible. Removal is `complete`'s job, once the
    /// dispatch has actually been handed to the dispatcher.
    #[test]
    fn test_approve_then_complete_removes() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(7));

        let taken = reg.approve(ProposalId(7));
        assert!(taken.is_some());
        assert!(reg.approve(ProposalId(7)).is_none(), "no second approval");
        assert_eq!(reg.list().len(), 1, "held until dispatch completes");

        assert!(reg.complete(ProposalId(7)));
        assert!(reg.list().is_empty());
        assert!(!reg.complete(ProposalId(7)));
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

    /// An approved proposal stays in the registry, tagged Approved.
    ///
    /// This is the whole point of the held state: while paused, the proposal
    /// must remain visible to `/control/approvals` instead of disappearing
    /// into a private queue where no operator could see or cancel it.
    #[test]
    fn test_approve_holds_entry_and_marks_state() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(11));

        let taken = reg.approve(ProposalId(11));
        assert!(taken.is_some());

        let listed = reg.list_with_state();
        assert_eq!(listed.len(), 1, "approved proposal must stay listed");
        assert_eq!(listed[0].1, ProposalState::Approved);
    }

    /// Approving twice must not queue a second execution of the same task.
    #[test]
    fn test_double_approve_returns_none() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(12));
        assert!(reg.approve(ProposalId(12)).is_some());
        assert!(reg.approve(ProposalId(12)).is_none());
    }

    /// An operator can reject an approval that is being held through a pause.
    #[test]
    fn test_reject_cancels_a_held_approval() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(13));
        let _ = reg.approve(ProposalId(13));

        assert!(
            reg.reject(ProposalId(13)),
            "held approval must be rejectable"
        );
        assert!(reg.list().is_empty());
        // Nothing left to flush on unpause -- the rejection stuck.
        assert!(reg.take_approved().is_empty());
    }

    /// Unpause flushes held approvals, and only the held ones.
    #[test]
    fn test_take_approved_returns_only_held_entries() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(14));
        let _ = reg.insert_for_test(proposal(15));
        let _ = reg.approve(ProposalId(14));

        let flushed = reg.take_approved();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].id, ProposalId(14));

        // The untouched pending proposal is still awaiting a decision.
        let left = reg.list_with_state();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1, ProposalState::Pending);
    }

    /// Regression: unpause and the approval channel must not both dispatch.
    ///
    /// The drain task dispatches only when `complete` returns true. Once
    /// `take_approved` has claimed the entry on unpause, a late delivery of
    /// the same proposal over the mpsc channel must find nothing to complete,
    /// or the approved task runs twice.
    #[test]
    fn test_complete_is_false_after_take_approved_claims_entry() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(16));
        let _ = reg.approve(ProposalId(16));

        assert_eq!(reg.take_approved().len(), 1);
        assert!(
            !reg.complete(ProposalId(16)),
            "entry already claimed; a second dispatch must be refused"
        );
    }

    /// Rejecting a held approval also blocks the drain task's dispatch.
    #[test]
    fn test_complete_is_false_after_reject() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(17));
        let _ = reg.approve(ProposalId(17));
        assert!(reg.reject(ProposalId(17)));
        assert!(!reg.complete(ProposalId(17)));
    }

    /// The TTL sweep must not discard a human's approval during a long pause.
    #[test]
    fn test_sweep_does_not_expire_approved_entries() {
        let reg = ApprovalRegistry::new(0); // everything is immediately expired
        let _ = reg.insert_for_test(proposal(18));
        let _ = reg.insert_for_test(proposal(19));
        let _ = reg.approve(ProposalId(18));

        let expired = reg.sweep_expired();
        assert_eq!(expired.len(), 1, "only the pending proposal expires");
        assert_eq!(expired[0].id, ProposalId(19));

        // The approved one survives and still dispatches on unpause.
        assert_eq!(reg.take_approved().len(), 1);
    }

    /// The in-room path takes the proposal outright, leaving nothing behind.
    #[test]
    fn test_approve_and_take_removes_entry() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(20));

        assert!(reg.approve_and_take(ProposalId(20)).is_some());
        assert!(reg.list().is_empty());
        assert!(reg.approve_and_take(ProposalId(20)).is_none());
    }

    /// An already-held approval is not re-takeable by the in-room path.
    #[test]
    fn test_approve_and_take_refuses_held_approval() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(21));
        let _ = reg.approve(ProposalId(21));
        assert!(reg.approve_and_take(ProposalId(21)).is_none());
    }

    /// While paused, a received approval is held rather than dispatched.
    #[test]
    fn test_drain_holds_while_paused() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(30));
        let _ = reg.approve(ProposalId(30));

        assert_eq!(
            decide_drain_action(true, &reg, ProposalId(30)),
            DrainAction::Hold
        );
        // Holding must not consume the entry -- it stays operator-visible.
        assert_eq!(reg.list().len(), 1);
    }

    /// Unpaused, the drain dispatches and consumes the entry exactly once.
    #[test]
    fn test_drain_dispatches_once_when_running() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(31));
        let _ = reg.approve(ProposalId(31));

        assert_eq!(
            decide_drain_action(false, &reg, ProposalId(31)),
            DrainAction::Dispatch
        );
        assert_eq!(
            decide_drain_action(false, &reg, ProposalId(31)),
            DrainAction::Skip,
            "a redelivery must not dispatch a second time"
        );
    }

    /// Regression: the unpause flush and the channel must not both dispatch.
    ///
    /// Sequence that used to double-execute an approved task: approve while
    /// paused, unpause (flush claims it), then the drain task finally reads the
    /// same proposal off the mpsc channel and sees a now-unpaused bridge.
    #[test]
    fn test_drain_skips_proposal_already_flushed_on_unpause() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(32));
        let _ = reg.approve(ProposalId(32));

        assert_eq!(
            decide_drain_action(true, &reg, ProposalId(32)),
            DrainAction::Hold
        );
        assert_eq!(reg.take_approved().len(), 1, "unpause flush claims it");

        assert_eq!(
            decide_drain_action(false, &reg, ProposalId(32)),
            DrainAction::Skip,
            "late channel delivery must not run the task again"
        );
    }

    /// An approval rejected during a hold never dispatches on unpause.
    #[test]
    fn test_drain_skips_proposal_rejected_while_held() {
        let reg = ApprovalRegistry::new(1800);
        let _ = reg.insert_for_test(proposal(33));
        let _ = reg.approve(ProposalId(33));
        assert!(reg.reject(ProposalId(33)));

        assert!(reg.take_approved().is_empty());
        assert_eq!(
            decide_drain_action(false, &reg, ProposalId(33)),
            DrainAction::Skip
        );
    }
}
