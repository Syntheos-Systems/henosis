//! synapse-session: SQLite-backed conversation persistence and cross-session search.
//!
//! Stores conversation transcripts (turns with role, content blocks, tool calls)
//! in a local SQLite database with FTS5 full-text search for cross-session recall.

mod store;

pub use store::{SearchResult, Session, SessionStore, Turn, UsageRecord, UsageTotals};
