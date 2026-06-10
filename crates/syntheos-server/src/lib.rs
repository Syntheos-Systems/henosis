#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-server
//!
//! The single HTTP entry point for the Henosis agent OS: Phase 0 unit 5 (the capstone).
//!
//! It boots the Phase 0 foundation -- the [`syntheos_axon`] bus, the
//! [`syntheos_identity`] principal directory, and the [`syntheos_dispatch`] dispatcher (running
//! the canonical deny-by-default gate chain, so every action is denied until real authorities
//! land) -- into shared [`AppState`] and serves a small surface: `/health`, `/version`,
//! `POST /enroll`, and `POST /dispatch`. The dispatch route runs an action through the real gate
//! chain, so the whole Phase 0 stack is exercised fail-closed end-to-end over the wire.
//!
//! Phase 1 kernel services mount under their own prefixes as they are extracted (Story 1.7).
//! Chiasm is wired: a persistent `ChiasmStore` opens at boot and `/chiasm/tasks` (+ `/chiasm/stats`)
//! make tasks queryable. Identity on the Chiasm surface is caller-asserted (`principal_id` in the
//! body/query) until PistisGate lands in Phase 3 -- same posture as `/dispatch`'s `RequestContext`.
//!
//! The surface is split into a library ([`router`] + [`AppState`]) so it can be unit-tested without
//! binding a socket; `main.rs` is the thin binary that wires state, initializes tracing, binds, and
//! serves with graceful shutdown.

pub mod app;

pub use app::{
    router, AppState, ChiasmCreateTask, ChiasmListQuery, ChiasmOwnerQuery, EnrollRequest,
};
