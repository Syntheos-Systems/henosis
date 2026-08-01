//! synapse-cron: Time-based agent task scheduling.
//!
//! Manages persistent cron jobs consumed by a host runtime. The managed
//! Henosis host routes due jobs through its governed Rift/Synapse room;
//! standalone callers may provide a different execution policy.

mod scheduler;

pub use scheduler::{CronJob, CronScheduler, JobConfig, JobResult};
