#![deny(missing_docs)]
//! Durable write-through sidecar for the in-process Axon bus.
//!
//! [`syntheos_axon::AxonBus`] stays exactly what it is: lossy, in-memory, live telemetry. This
//! crate provides durable delivery: [`DurableAxonBus`] wraps the in-process bus and
//! persists every envelope to SQLite BEFORE fanning it out, giving audit-grade durability,
//! cursor-based consumption, and replay -- without putting a database write on the hot path of
//! consumers that only need telemetry (they keep using the in-process bus directly).
//!
//! Ordering contract: the SQLite append is the record; the live fanout is best-effort delivery
//! of it. A publish that cannot be persisted is an error and is NOT fanned out -- an audit
//! consumer must never observe an event that has no durable row.
//!
//! Cursors are named: one position per (consumer, tenant, channel), advanced atomically with
//! each [`DurableAxonBus::consume`] batch. Replay is positional and read-only: any range of the
//! log can be re-read by `seq` without touching cursors. `seq` is monotonic and never reused,
//! so positions stay valid across [`DurableAxonBus::prune_before`].

mod durable;
mod error;

pub use durable::{DurableAxonBus, StoredEnvelope};
pub use error::DurableAxonError;
