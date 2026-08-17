//! Runtime-only capability used to reject stale managed-room leaders.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Complete generation identity issued to one managed-room leader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomFence {
    /// Rift server whose managed effects this capability may authorize.
    pub server_id: Uuid,
    /// Positive monotonically increasing generation stored in PostgreSQL.
    pub epoch: i64,
    /// Opaque random value preventing an epoch alone from becoming a credential.
    pub lease_id: Uuid,
}

/// Validates fence shape and compares it with one durable room-state row.
impl RoomFence {
    /// Return whether every fence component exactly matches a valid durable row.
    pub fn matches(&self, server_id: Uuid, epoch: i64, lease_id: Option<Uuid>) -> bool {
        self.epoch > 0
            && !self.lease_id.is_nil()
            && self.server_id == server_id
            && self.epoch == epoch
            && lease_id == Some(self.lease_id)
    }
}

#[cfg(test)]
/// Covers complete fence identity semantics before persistence is involved.
mod tests {
    use uuid::Uuid;

    use super::RoomFence;

    /// A room fence is identified by its room, epoch, and opaque lease together.
    #[test]
    fn room_fence_identity_requires_every_component() {
        let server_id = Uuid::new_v4();
        let lease_id = Uuid::new_v4();
        let fence = RoomFence {
            server_id,
            epoch: 7,
            lease_id,
        };

        assert!(fence.matches(server_id, 7, Some(lease_id)));
        assert!(!fence.matches(Uuid::new_v4(), 7, Some(lease_id)));
        assert!(!fence.matches(server_id, 8, Some(lease_id)));
        assert!(!fence.matches(server_id, 7, Some(Uuid::new_v4())));
        assert!(!fence.matches(server_id, 7, None));
        assert!(!RoomFence { epoch: 0, ..fence }.matches(server_id, 0, Some(lease_id)));
        assert!(
            !RoomFence {
                lease_id: Uuid::nil(),
                ..fence
            }
            .matches(server_id, 7, Some(Uuid::nil()))
        );
    }
}
