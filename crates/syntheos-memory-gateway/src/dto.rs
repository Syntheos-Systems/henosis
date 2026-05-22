//! Data transfer objects for the `frameshift-memory-http` wire contract.
//! These mirror the shapes documented in that crate's WIRE.md exactly.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inbound store request: free text plus tags and arbitrary metadata.
#[derive(Debug, Deserialize)]
pub struct StoreRequest {
    /// The memory content to store.
    pub text: String,
    /// Caller-supplied tags (optional; defaults to empty).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Caller-supplied metadata (optional; defaults to empty).
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Response to a successful store: opaque id plus creation timestamp.
#[derive(Debug, Serialize)]
pub struct StoreResponse {
    /// Opaque memory id (a Kleos i64 encoded as a decimal string).
    pub id: String,
    /// RFC3339 creation timestamp reported by Kleos.
    pub created_at: String,
}

/// Inbound search request.
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    /// Free-text query.
    pub query: String,
    /// Maximum number of results (defaults via [`default_k`]).
    #[serde(default = "default_k")]
    pub k: usize,
    /// Optional filters narrowing the search.
    #[serde(default)]
    pub filters: Filters,
}

/// Default result count when the caller omits `k`.
fn default_k() -> usize {
    10
}

/// Optional search filters carried in a search request.
#[derive(Debug, Default, Deserialize)]
pub struct Filters {
    /// Restrict to memories carrying all of these tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Lower time bound (RFC3339); currently advisory.
    pub after: Option<String>,
    /// Upper time bound (RFC3339); currently advisory.
    pub before: Option<String>,
    /// Metadata equality filters; currently advisory.
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Search response wrapper.
#[derive(Debug, Serialize)]
pub struct SearchResponse {
    /// Matching memories in wire shape.
    pub results: Vec<Memory>,
}

/// List response wrapper.
#[derive(Debug, Serialize)]
pub struct ListResponse {
    /// Listed memories in wire shape.
    pub items: Vec<Memory>,
}

/// Health response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Whether the upstream Kleos instance is reachable.
    pub healthy: bool,
    /// Human-readable status detail.
    pub message: String,
}

/// A memory in the wire shape; metadata is flattened into the object so there
/// is no nested `metadata` key, per the contract.
#[derive(Debug, Serialize)]
pub struct Memory {
    /// Opaque memory id (Kleos i64 as a decimal string).
    pub id: String,
    /// The memory content.
    pub text: String,
    /// Associated tags.
    pub tags: Vec<String>,
    /// RFC3339 creation timestamp, if known.
    pub created_at: Option<String>,
    /// RFC3339 update timestamp, if known.
    pub updated_at: Option<String>,
    /// Additional Kleos fields surfaced as flattened metadata.
    #[serde(flatten)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}
