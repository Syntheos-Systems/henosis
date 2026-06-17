//! Concurrent multi-session runtime.
//!
//! `SessionManager` owns N independent agent sessions, each driving the
//! existing `agent_turn_with_pricing` stream on its own tokio task. Events are
//! fanned out via a `broadcast` channel so any renderer can watch every
//! session at once.

pub mod manager;
pub mod types;
pub mod worktree;

#[cfg(test)]
mod test_support;

pub use manager::SessionManager;
pub use types::{SessionEvent, SessionId, SessionSnapshot, SessionStatus};
pub use worktree::{SessionWorktree, prepare as prepare_worktree, remove as remove_worktree};
