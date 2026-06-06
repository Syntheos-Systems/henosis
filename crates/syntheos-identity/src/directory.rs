//! The principal directory trait gates resolve against, plus the Phase 0 in-memory
//! implementation.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use syntheos_contracts::{Principal, PrincipalId, PrincipalKind};

use crate::error::DirectoryError;

/// The canonical actor registry. A gate turns the `PrincipalId` carried in a
/// [`syntheos_contracts::GateRequest`] into a full [`Principal`] by consulting this.
///
/// Async and `Result`-returning so a storage-backed implementation (the unit-6 DB decision)
/// drops in without changing call sites. Object-safe via `async_trait`, so it can be held as
/// `Arc<dyn PrincipalDirectory>`.
#[async_trait]
pub trait PrincipalDirectory: Send + Sync {
    /// Enroll a new actor, minting a fresh [`PrincipalId`], and return the created [`Principal`].
    async fn enroll(
        &self,
        kind: PrincipalKind,
        display: Option<String>,
    ) -> Result<Principal, DirectoryError>;

    /// Look up an actor by id. `Ok(None)` if no such principal is enrolled.
    async fn lookup(&self, id: PrincipalId) -> Result<Option<Principal>, DirectoryError>;

    /// List every enrolled actor (order unspecified).
    async fn list(&self) -> Result<Vec<Principal>, DirectoryError>;
}

/// The Phase 0 in-memory [`PrincipalDirectory`]: a process-local map, no persistence.
///
/// Share it as `Arc<InMemoryDirectory>`; all methods take `&self`. Like the Axon bus it uses a
/// std `RwLock` -- no `.await` is ever held across the lock.
pub struct InMemoryDirectory {
    /// The enrolled principals, keyed by id.
    principals: RwLock<HashMap<PrincipalId, Principal>>,
}

impl InMemoryDirectory {
    /// Create an empty directory.
    pub fn new() -> Self {
        Self {
            principals: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryDirectory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PrincipalDirectory for InMemoryDirectory {
    async fn enroll(
        &self,
        kind: PrincipalKind,
        display: Option<String>,
    ) -> Result<Principal, DirectoryError> {
        let principal = Principal {
            id: PrincipalId::new(),
            kind,
            display,
        };
        let mut map = self.principals.write().unwrap_or_else(|e| e.into_inner());
        map.insert(principal.id, principal.clone());
        Ok(principal)
    }

    async fn lookup(&self, id: PrincipalId) -> Result<Option<Principal>, DirectoryError> {
        let map = self.principals.read().unwrap_or_else(|e| e.into_inner());
        Ok(map.get(&id).cloned())
    }

    async fn list(&self) -> Result<Vec<Principal>, DirectoryError> {
        let map = self.principals.read().unwrap_or_else(|e| e.into_inner());
        Ok(map.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn enroll_then_lookup() {
        let dir = InMemoryDirectory::new();
        let p = dir
            .enroll(PrincipalKind::Agent, Some("eidolon".into()))
            .await
            .expect("enroll");
        let got = dir.lookup(p.id).await.expect("lookup").expect("present");
        assert_eq!(got, p);
    }

    #[tokio::test]
    async fn lookup_unknown_is_none() {
        let dir = InMemoryDirectory::new();
        assert!(dir
            .lookup(PrincipalId::new())
            .await
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn enroll_mints_unique_ids() {
        let dir = InMemoryDirectory::new();
        let a = dir.enroll(PrincipalKind::Human, None).await.expect("enroll");
        let b = dir.enroll(PrincipalKind::Human, None).await.expect("enroll");
        assert_ne!(a.id, b.id);
    }

    #[tokio::test]
    async fn kind_and_display_preserved() {
        let dir = InMemoryDirectory::new();
        let p = dir
            .enroll(PrincipalKind::Service, Some("hermes".into()))
            .await
            .expect("enroll");
        let got = dir.lookup(p.id).await.expect("lookup").expect("present");
        assert_eq!(got.kind, PrincipalKind::Service);
        assert_eq!(got.display.as_deref(), Some("hermes"));
    }

    #[tokio::test]
    async fn list_returns_all_enrolled() {
        let dir = InMemoryDirectory::new();
        dir.enroll(PrincipalKind::Agent, None).await.expect("enroll");
        dir.enroll(PrincipalKind::Human, None).await.expect("enroll");
        dir.enroll(PrincipalKind::Service, None)
            .await
            .expect("enroll");
        assert_eq!(dir.list().await.expect("list").len(), 3);
    }

    #[tokio::test]
    async fn usable_as_trait_object() {
        let dir: Arc<dyn PrincipalDirectory> = Arc::new(InMemoryDirectory::new());
        let p = dir
            .enroll(PrincipalKind::Integration, Some("github".into()))
            .await
            .expect("enroll");
        let got = dir.lookup(p.id).await.expect("lookup").expect("present");
        assert_eq!(got, p);
    }
}
