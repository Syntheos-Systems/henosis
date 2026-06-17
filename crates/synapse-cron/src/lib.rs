//! synapse-cron: Time-based agent task scheduling.
//!
//! Manages cron jobs that spawn Synapse agent sessions on a schedule.
//! Jobs are stored in ~/.synapse/cron/jobs.json and executed by a
//! background tick loop.

mod scheduler;

pub use scheduler::{CronJob, CronScheduler, JobConfig, JobResult};
