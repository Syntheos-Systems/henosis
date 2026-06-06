#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-server
//!
//! The single HTTP entry point for the Henosis agent OS: Phase 0 unit 5 (the capstone).
//!
//! It boots the Phase 0 foundation -- the [`syntheos_axon`] bus, the
//! [`syntheos_identity`] principal directory, and the [`syntheos_dispatch`] dispatcher (running
//! the stubbed canonical gate chain) -- into shared [`AppState`] and serves a small surface:
//! `/health`, `/version`, `POST /enroll`, and `POST /dispatch`. The dispatch route runs an action
//! through the real gate chain and executes it, so the whole Phase 0 stack is exercised end-to-end
//! over the wire.
//!
//! The surface is split into a library ([`router`] + [`AppState`]) so it can be unit-tested without
//! binding a socket; `main.rs` is the thin binary that wires state, initializes tracing, binds, and
//! serves with graceful shutdown.

pub mod app;

pub use app::{router, AppState, EnrollRequest};
