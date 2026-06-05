#![deny(missing_docs)]
#![warn(clippy::all)]
//! # syntheos-axon
//!
//! The in-process pub/sub backbone for the Henosis agent OS: Phase 0 unit 2.
//!
//! A single [`AxonBus`] (held as `Arc<AxonBus>`) carries [`syntheos_contracts::AxonEnvelope`]s
//! between services running in the same process, over `tokio::sync::broadcast`. Services can use
//! the raw [`AxonBus::publish`] / [`AxonBus::subscribe`] API, or the typed sugar
//! ([`AxonBus::publish_event`] / [`AxonBus::subscribe_typed`]) that leans on the
//! [`syntheos_contracts::TypedEvent`] contract for type safety.
//!
//! ## What this is not
//!
//! No persistence, replay, cursors, retention, webhooks, or cross-process delivery, and no
//! delivery guarantee under backpressure (a lagging subscriber is signalled and resumes -- see
//! [`AxonError::Lagged`]). The durable Kleos axon is reconciled later, at service-absorption time.

pub mod bus;
pub mod error;
pub mod typed;

pub use bus::AxonBus;
pub use error::AxonError;
pub use typed::TypedReceiver;
