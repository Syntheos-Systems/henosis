#[cfg(feature = "ml")]
pub mod lance;

#[cfg(feature = "ml")]
pub use lance::LanceIndex;

use crate::Result;
use async_trait::async_trait;

/// Name of the primary (whole-memory) vector table.
pub const DEFAULT_TABLE_NAME: &str = "memory_vectors";

/// Name of the chunk-level vector table.
pub const CHUNK_TABLE_NAME: &str = "memory_chunk_vectors";

/// Schema version for the LanceDB vector tables. Bump when changing the
/// column layout (not when changing the embedding model or dimensions --
/// those are validated dynamically at open time).
pub const LANCE_SCHEMA_VERSION: u32 = 2;

/// Minimum number of rows before an IVF_HNSW_PQ index is built. Below this
/// threshold LanceDB's IVF clustering does not converge cleanly and a linear
/// scan over the fixed-size-list is faster anyway, so we skip index creation.
/// Lives here (not in `lance`) so ml-off consumers can still report it.
pub const MIN_ROWS_FOR_INDEX: usize = 256;

#[derive(Debug, Clone)]
/// One ANN search hit: the memory id, its distance, and result rank.
pub struct VectorHit {
    pub memory_id: i64,
    pub distance: Option<f32>,
    pub rank: usize,
}

#[async_trait]
/// Object-safe ANN index backend. The only concrete impl (LanceIndex) is
/// ml-gated; consumers hold `Option<Arc<dyn VectorIndex>>` and degrade to
/// FTS + sqlite-vec when absent.
pub trait VectorIndex: Send + Sync {
    /// Upsert one memory embedding.
    async fn insert(&self, memory_id: i64, embedding: &[f32]) -> Result<()>;
    /// ANN search for the nearest `limit` memories.
    async fn search(&self, embedding: &[f32], limit: usize) -> Result<Vec<VectorHit>>;
    /// Remove a memory's embedding from the index.
    async fn delete(&self, memory_id: i64) -> Result<()>;
    /// Number of rows currently in the index.
    async fn count(&self) -> Result<usize>;

    /// Batched upsert. Default impl loops `insert`; LanceIndex overrides with
    /// a single delete+add round trip so a backfill pass paying the
    /// O(table_size) delete-scan once per call instead of once per row.
    async fn insert_many(&self, items: &[(i64, Vec<f32>)]) -> Result<()> {
        for (id, emb) in items {
            self.insert(*id, emb).await?;
        }
        Ok(())
    }

    /// Force a rebuild of the ANN index. `replace=true` drops any existing
    /// index before rebuilding. Returns `true` if the index was (re)built,
    /// `false` if the operation was skipped (below row threshold, or an
    /// existing index satisfied `replace=false`).
    async fn rebuild_index(&self, replace: bool) -> Result<bool>;
}
