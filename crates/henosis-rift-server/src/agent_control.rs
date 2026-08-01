//! Object-safe boundary between Rift desired state and Henosis execution control.

use std::sync::{Arc, OnceLock};

use uuid::Uuid;

use crate::error::AppError;
use crate::models::agent_control::{AgentSeatInput, ExecutionCapabilityCatalog};

/// Sanitized failures returned by the managed Henosis runtime controller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedAgentControlError {
    /// A requested harness, model, or typed setting is unavailable.
    #[error("{0}")]
    CapabilityUnavailable(String),
    /// A requested opaque credential binding is not currently usable.
    #[error("{0}")]
    CredentialNotReady(String),
    /// The managed runtime could not complete an internal operation.
    #[error("{0}")]
    Internal(String),
}

/// Maps sanitized managed-control failures into stable Rift API errors.
impl From<ManagedAgentControlError> for AppError {
    /// Preserve stable public failure categories without exposing executor details.
    fn from(error: ManagedAgentControlError) -> Self {
        match error {
            ManagedAgentControlError::CapabilityUnavailable(message) => {
                AppError::capability_unavailable(message)
            }
            ManagedAgentControlError::CredentialNotReady(message) => {
                AppError::credential_not_ready(message)
            }
            ManagedAgentControlError::Internal(message) => AppError::Internal(message),
        }
    }
}

/// Host-owned operations Rift needs to validate and reconcile desired revisions.
#[async_trait::async_trait]
pub trait ManagedAgentControl: Send + Sync {
    /// Return the current secret-free execution catalog for one human owner.
    async fn capabilities(
        &self,
        server_id: Uuid,
        owner_user_id: Uuid,
    ) -> Result<ExecutionCapabilityCatalog, ManagedAgentControlError>;

    /// Validate every seat against host capabilities and credential readiness.
    async fn validate_revision(
        &self,
        server_id: Uuid,
        owner_user_id: Uuid,
        seats: &[AgentSeatInput],
    ) -> Result<(), ManagedAgentControlError>;

    /// Notify the supervisor after an immutable desired revision commits.
    async fn revision_committed(
        &self,
        server_id: Uuid,
        revision: i64,
    ) -> Result<(), ManagedAgentControlError>;

    /// Ask the supervisor to retry an already durable desired revision.
    async fn retry_revision(
        &self,
        server_id: Uuid,
        revision: i64,
    ) -> Result<(), ManagedAgentControlError>;
}

/// A cloneable, one-time installation point for the managed runtime controller.
#[derive(Clone, Default)]
pub struct ManagedAgentControlRegistry {
    controller: Arc<OnceLock<Arc<dyn ManagedAgentControl>>>,
}

/// Failure returned when a second controller tries to replace the installed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("managed agent controller is already installed")]
pub struct ControllerAlreadyInstalled;

/// Installs and resolves the managed controller shared by all Rift handlers.
impl ManagedAgentControlRegistry {
    /// Install the controller exactly once across every clone of this registry.
    pub fn install(
        &self,
        controller: Arc<dyn ManagedAgentControl>,
    ) -> Result<(), ControllerAlreadyInstalled> {
        self.controller
            .set(controller)
            .map_err(|_| ControllerAlreadyInstalled)
    }

    /// Resolve the controller or return Rift's stable standalone-mode failure.
    pub fn controller(&self) -> Result<Arc<dyn ManagedAgentControl>, AppError> {
        self.controller
            .get()
            .cloned()
            .ok_or_else(AppError::managed_runtime_unavailable)
    }

    /// Report whether Henosis installed a controller before serving requests.
    pub fn is_installed(&self) -> bool {
        self.controller.get().is_some()
    }
}

#[cfg(test)]
/// Exercises one-time installation, standalone failure, and object-safe dispatch.
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::models::agent_control::ExecutionCapabilityCatalog;

    use super::*;

    /// Fake controller that records capability calls through the trait object.
    struct FakeControl {
        calls: AtomicUsize,
    }

    /// Implements every managed operation without touching a real executor host.
    #[async_trait::async_trait]
    impl ManagedAgentControl for FakeControl {
        /// Return an empty generation-stamped catalog and record the call.
        async fn capabilities(
            &self,
            _server_id: Uuid,
            _owner_user_id: Uuid,
        ) -> Result<ExecutionCapabilityCatalog, ManagedAgentControlError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutionCapabilityCatalog {
                generation: Uuid::nil(),
                harnesses: Vec::new(),
            })
        }

        /// Accept all fake seat revisions.
        async fn validate_revision(
            &self,
            _server_id: Uuid,
            _owner_user_id: Uuid,
            _seats: &[AgentSeatInput],
        ) -> Result<(), ManagedAgentControlError> {
            Ok(())
        }

        /// Accept fake commit notifications.
        async fn revision_committed(
            &self,
            _server_id: Uuid,
            _revision: i64,
        ) -> Result<(), ManagedAgentControlError> {
            Ok(())
        }

        /// Accept fake retry notifications.
        async fn retry_revision(
            &self,
            _server_id: Uuid,
            _revision: i64,
        ) -> Result<(), ManagedAgentControlError> {
            Ok(())
        }
    }

    /// Build a fresh fake controller for registry tests.
    fn fake_control() -> Arc<FakeControl> {
        Arc::new(FakeControl {
            calls: AtomicUsize::new(0),
        })
    }

    /// Empty standalone registries fail with the stable HTTP 503 category.
    #[test]
    fn empty_registry_is_unavailable() {
        let error = match ManagedAgentControlRegistry::default().controller() {
            Ok(_) => panic!("empty registry unexpectedly returned a controller"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            AppError::Coded {
                status: axum::http::StatusCode::SERVICE_UNAVAILABLE,
                code: "managed_runtime_unavailable",
                ..
            }
        ));
    }

    /// Installation through one clone is visible to the clone held by the router.
    #[tokio::test]
    async fn controller_can_be_installed_before_serving() {
        let installer = ManagedAgentControlRegistry::default();
        let router_registry = installer.clone();
        let fake = fake_control();
        assert!(!router_registry.is_installed());
        installer.install(fake.clone()).unwrap();
        assert!(router_registry.is_installed());

        let catalog = router_registry
            .controller()
            .unwrap()
            .capabilities(Uuid::new_v4(), Uuid::new_v4())
            .await
            .unwrap();
        assert_eq!(catalog.generation, Uuid::nil());
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    /// A second install cannot replace the controller already visible to handlers.
    #[test]
    fn duplicate_installation_is_denied() {
        let registry = ManagedAgentControlRegistry::default();
        registry.install(fake_control()).unwrap();
        assert_eq!(
            registry.install(fake_control()).unwrap_err(),
            ControllerAlreadyInstalled
        );
    }

    /// Public error mapping retains capability and credential categories.
    #[test]
    fn managed_errors_map_to_stable_api_codes() {
        let cases = [
            (
                ManagedAgentControlError::CapabilityUnavailable("missing model".to_string()),
                "capability_unavailable",
            ),
            (
                ManagedAgentControlError::CredentialNotReady("missing binding".to_string()),
                "credential_not_ready",
            ),
        ];
        for (managed, expected_code) in cases {
            assert!(matches!(
                AppError::from(managed),
                AppError::Coded { code, .. } if code == expected_code
            ));
        }
    }
}
