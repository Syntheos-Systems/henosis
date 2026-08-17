//! Monotonic managed-room leadership verification and cancellation.

use std::sync::Arc;

use henosis_rift_server::models::leadership::RoomFence;
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::BridgeError;
use crate::rift_client::RiftRestClient;

/// Shared fail-closed gate for one managed-room bridge generation.
///
/// Revocation is monotonic: once verification fails, every clone remains
/// revoked. Aborting a local executor prevents further work in that Tokio
/// task, but cannot roll back arbitrary external side effects it already
/// started before revocation.
#[derive(Clone)]
pub struct LeadershipGuard {
    /// Rift client used for authoritative managed-room status verification.
    rift: Option<Arc<RiftRestClient>>,
    /// Managed server whose durable fence must still be current.
    server_id: Option<Uuid>,
    /// Monotonic cancellation signal; `true` means permanently revoked.
    revoked: watch::Sender<bool>,
}

/// Runtime-owned lease whose destruction revokes every retained guard clone.
pub(crate) struct LeadershipRevocationOnDrop {
    /// Shared generation guard revoked when the runtime future is dropped.
    leadership: Arc<LeadershipGuard>,
}

/// Arms revocation for every exit path, including force-aborting the runtime task.
impl LeadershipRevocationOnDrop {
    /// Attach lifetime revocation to the runtime's authoritative guard.
    pub(crate) fn new(leadership: Arc<LeadershipGuard>) -> Self {
        Self { leadership }
    }
}

/// Force retained work to observe cancellation when its owning runtime disappears.
impl Drop for LeadershipRevocationOnDrop {
    /// Revoke before any detached task can continue past runtime destruction.
    fn drop(&mut self) {
        self.leadership.revoke();
    }
}

/// Construction, verification, and cancellation for managed leadership.
impl LeadershipGuard {
    /// Build a guard that verifies the supplied managed fence through Rift.
    ///
    /// Passing no fence keeps standalone bridge behavior unmanaged and avoids
    /// all verification traffic.
    pub fn new(rift: Arc<RiftRestClient>, managed_fence: Option<RoomFence>) -> Self {
        let (revoked, _receiver) = watch::channel(false);
        Self {
            rift: managed_fence.map(|_| rift),
            server_id: managed_fence.map(|fence| fence.server_id),
            revoked,
        }
    }

    /// Build a standalone no-op guard with no Rift dependency.
    pub fn unmanaged() -> Self {
        let (revoked, _receiver) = watch::channel(false);
        Self {
            rift: None,
            server_id: None,
            revoked,
        }
    }

    /// Verify the managed generation and return the room's current pause bit.
    ///
    /// Any verification, transport, or server error permanently revokes the
    /// guard. Unmanaged guards return `false` without performing I/O.
    pub async fn require_current(&self) -> Result<bool, BridgeError> {
        if self.is_revoked() {
            return Err(BridgeError::StaleLeadership);
        }
        let (Some(rift), Some(server_id)) = (&self.rift, self.server_id) else {
            return Ok(false);
        };

        let revocation = wait_for_revocation(self.subscribe());
        tokio::pin!(revocation);
        let status = tokio::select! {
            status = rift.is_paused(server_id) => status,
            _ = &mut revocation => return Err(BridgeError::StaleLeadership),
        };
        let paused = match status {
            Ok(paused) => paused,
            Err(error) => {
                tracing::error!(error = %error, "managed-room leadership verification failed");
                self.revoke();
                return Err(BridgeError::StaleLeadership);
            }
        };

        if self.is_revoked() {
            Err(BridgeError::StaleLeadership)
        } else {
            Ok(paused)
        }
    }

    /// Return whether this guard represents a managed room.
    pub fn is_managed(&self) -> bool {
        self.server_id.is_some()
    }

    /// Return whether leadership has been permanently revoked.
    pub fn is_revoked(&self) -> bool {
        *self.revoked.borrow()
    }

    /// Subscribe to monotonic cancellation; `true` means permanently revoked.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.revoked.subscribe()
    }

    /// Permanently revoke this generation and wake every cancellation waiter.
    pub(crate) fn revoke(&self) {
        if !self.revoked.send_replace(true) {
            tracing::error!("managed-room leadership revoked");
        }
    }
}

/// Wait for revocation while also observing a value set before subscription.
pub(crate) async fn wait_for_revocation(mut receiver: watch::Receiver<bool>) {
    let _ = receiver.wait_for(|revoked| *revoked).await;
}

#[cfg(test)]
/// Covers unmanaged compatibility and monotonic explicit revocation.
mod tests {
    use std::sync::Arc;

    use henosis_rift_server::models::leadership::RoomFence;
    use uuid::Uuid;

    use crate::auth::AgentAuthManager;
    use crate::rift_client::RiftRestClient;

    use super::{wait_for_revocation, LeadershipGuard, LeadershipRevocationOnDrop};

    /// An unmanaged guard succeeds without a Rift dependency until explicitly revoked.
    #[tokio::test]
    async fn unmanaged_guard_is_current_until_revoked() {
        let guard = LeadershipGuard::unmanaged();

        assert!(!guard.is_managed());
        assert!(!guard.is_revoked());
        assert!(!guard.require_current().await.expect("unmanaged guard"));

        guard.revoke();

        assert!(guard.is_revoked());
        assert!(guard.require_current().await.is_err());
    }

    /// A managed verification error permanently flips every cancellation receiver.
    #[tokio::test]
    async fn managed_verification_error_permanently_revokes() {
        let closed_socket = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind disposable socket")
            .local_addr()
            .expect("read disposable address");
        let public_socket = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind public disposable socket")
            .local_addr()
            .expect("read public disposable address");
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 1,
            lease_id: Uuid::new_v4(),
        };
        let auth =
            AgentAuthManager::new("j".repeat(32), "b".repeat(32)).with_managed_fence(Some(fence));
        let rift = Arc::new(
            RiftRestClient::new(
                format!("http://{public_socket}"),
                format!("http://{closed_socket}"),
                auth,
            )
            .expect("construct client"),
        );
        let guard = LeadershipGuard::new(rift, Some(fence));
        let receiver = guard.subscribe();

        assert!(guard.require_current().await.is_err());
        assert!(guard.is_revoked());
        assert!(*receiver.borrow());
        assert!(guard.require_current().await.is_err());
    }

    /// Explicit revocation cancels an already-connected status verification promptly.
    #[tokio::test]
    async fn revocation_interrupts_an_in_flight_status_request() {
        let private_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent private Rift peer");
        let private_address = private_listener
            .local_addr()
            .expect("read private peer address");
        let public_address = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind disposable public address")
            .local_addr()
            .expect("read public peer address");
        let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
        let silent_peer = tokio::spawn(async move {
            let (_socket, _) = private_listener
                .accept()
                .await
                .expect("accept status request");
            let _ = connected_tx.send(());
            std::future::pending::<()>().await;
        });
        let fence = RoomFence {
            server_id: Uuid::new_v4(),
            epoch: 2,
            lease_id: Uuid::new_v4(),
        };
        let auth =
            AgentAuthManager::new("j".repeat(32), "b".repeat(32)).with_managed_fence(Some(fence));
        let rift = Arc::new(
            RiftRestClient::new(
                format!("http://{public_address}"),
                format!("http://{private_address}"),
                auth,
            )
            .expect("construct client"),
        );
        let guard = Arc::new(LeadershipGuard::new(rift, Some(fence)));
        let verification = tokio::spawn({
            let guard = guard.clone();
            async move { guard.require_current().await }
        });
        connected_rx
            .await
            .expect("status verification must reach the silent peer");

        guard.revoke();

        let result = tokio::time::timeout(std::time::Duration::from_millis(100), verification)
            .await
            .expect("revocation must interrupt the in-flight request")
            .expect("verification task must join");
        assert!(matches!(
            result,
            Err(crate::error::BridgeError::StaleLeadership)
        ));
        silent_peer.abort();
    }

    /// A waiter created after revocation still resolves from the current watch value.
    #[tokio::test]
    async fn fresh_waiter_observes_revocation_that_already_happened() {
        let guard = LeadershipGuard::unmanaged();
        guard.revoke();

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_for_revocation(guard.subscribe()),
        )
        .await
        .expect("an already-revoked guard must wake a fresh waiter");
    }

    /// Dropping the runtime-owned lease revokes clones retained by detached work.
    #[test]
    fn runtime_owner_drop_revokes_every_guard_clone() {
        let guard = Arc::new(LeadershipGuard::unmanaged());
        let retained_by_work = guard.clone();
        let owner = LeadershipRevocationOnDrop::new(guard);

        drop(owner);

        assert!(retained_by_work.is_revoked());
    }
}
