//! Knowledge graph -- entity storage, relationships, pagerank, community detection, and search.

/// Graph construction from entity/relationship data.
pub mod builder;
/// Community detection and Louvain clustering.
pub mod communities;
/// Entity co-occurrence tracking and scoring.
pub mod cooccurrence;
/// Entity CRUD -- create, read, update, delete graph nodes.
pub mod entities;
/// PageRank computation over the entity graph.
pub mod pagerank;
/// Semantic and keyword search across graph entities.
pub mod search;
/// Shared types for graph operations.
pub mod types;
