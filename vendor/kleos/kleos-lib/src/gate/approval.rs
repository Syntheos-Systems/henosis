/// In-memory record of a pending tool approval, held in AppState until
/// the user responds or the timeout fires.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub gate_id: i64,
    pub agent: String,
    pub tool_name: String,
    pub command: String,
    pub created_at: std::time::Instant,
}

/// Remove all approvals that have exceeded APPROVAL_TIMEOUT_SECS from the map.
/// Call this periodically to prevent stale entries accumulating.
pub fn cleanup_expired_approvals(
    approvals: &mut std::collections::HashMap<
        i64,
        (PendingApproval, tokio::sync::oneshot::Sender<bool>),
    >,
) {
    use crate::gate::APPROVAL_TIMEOUT_SECS;
    let now = std::time::Instant::now();
    approvals.retain(|_, (pending, _)| {
        now.duration_since(pending.created_at).as_secs() < APPROVAL_TIMEOUT_SECS
    });
}
