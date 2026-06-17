//! Cron scheduler: manages job definitions, persistence, and tick-based execution.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use cron::Schedule;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Configuration for a single cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobConfig {
    /// Unique job identifier.
    pub id: String,
    /// Cron expression (e.g. "0 */6 * * *" for every 6 hours).
    pub schedule: String,
    /// The task/prompt to send to the agent.
    pub task: String,
    /// Working directory for the agent (defaults to home dir).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Model override (uses default if not set).
    #[serde(default)]
    pub model: Option<String>,
    /// Provider override.
    #[serde(default)]
    pub provider: Option<String>,
    /// Max turns for the agent (default 15).
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// Whether the job is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Returns the default maximum number of agent turns for a cron job.
fn default_max_turns() -> usize {
    15
}
/// Returns the default enabled state for a cron job.
fn default_true() -> bool {
    true
}

/// A cron job with parsed schedule and runtime state.
#[derive(Debug, Clone)]
pub struct CronJob {
    pub config: JobConfig,
    /// Parsed cron schedule.
    schedule: Schedule,
    /// Last time this job was executed (unix seconds).
    pub last_run: Option<i64>,
    /// Number of times this job has run.
    pub run_count: u64,
}

/// Result from a job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub job_id: String,
    pub started_at: i64,
    pub finished_at: i64,
    pub output: String,
    pub success: bool,
}

// ---------------------------------------------------------------------------
// CronScheduler
// ---------------------------------------------------------------------------

/// Manages cron jobs, persistence, and tick-based scheduling.
pub struct CronScheduler {
    jobs: Vec<CronJob>,
    jobs_path: PathBuf,
    results_path: PathBuf,
}

/// Adds constructors, persistence, and scheduling behavior for `CronScheduler`.
impl CronScheduler {
    /// Create a new scheduler, loading jobs from ~/.synapse/cron/jobs.json.
    pub fn open_default() -> Result<Self> {
        let base = dirs::home_dir()
            .context("no home dir")?
            .join(".synapse")
            .join("cron");
        std::fs::create_dir_all(&base).context("create cron dir")?;

        let jobs_path = base.join("jobs.json");
        let results_path = base.join("results.json");

        let mut scheduler = Self {
            jobs: Vec::new(),
            jobs_path,
            results_path,
        };
        scheduler.load()?;
        Ok(scheduler)
    }

    /// Create a scheduler from a specific directory.
    pub fn open(base: &Path) -> Result<Self> {
        std::fs::create_dir_all(base).context("create cron dir")?;

        let jobs_path = base.join("jobs.json");
        let results_path = base.join("results.json");

        let mut scheduler = Self {
            jobs: Vec::new(),
            jobs_path,
            results_path,
        };
        scheduler.load()?;
        Ok(scheduler)
    }

    /// Load jobs from disk.
    fn load(&mut self) -> Result<()> {
        if !self.jobs_path.exists() {
            return Ok(());
        }

        let data = std::fs::read_to_string(&self.jobs_path).context("read jobs.json")?;
        let configs: Vec<JobConfig> = serde_json::from_str(&data).context("parse jobs.json")?;

        self.jobs.clear();
        for config in configs {
            match config.schedule.parse::<Schedule>() {
                Ok(schedule) => {
                    self.jobs.push(CronJob {
                        config,
                        schedule,
                        last_run: None,
                        run_count: 0,
                    });
                }
                Err(e) => {
                    log::warn!(
                        "invalid cron expression '{}' for job '{}': {e}",
                        config.schedule,
                        config.id
                    );
                }
            }
        }

        Ok(())
    }

    /// Save jobs to disk.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.jobs_path.parent() {
            std::fs::create_dir_all(parent).context("create cron dir")?;
        }
        let configs: Vec<&JobConfig> = self.jobs.iter().map(|j| &j.config).collect();
        let json = serde_json::to_string_pretty(&configs).context("serialize jobs")?;
        std::fs::write(&self.jobs_path, json).context("write jobs.json")?;
        Ok(())
    }

    /// Add a new job. Returns error if the cron expression is invalid.
    pub fn add_job(&mut self, config: JobConfig) -> Result<()> {
        let schedule: Schedule = config.schedule.parse().context("invalid cron expression")?;

        // Remove existing job with same ID
        self.jobs.retain(|j| j.config.id != config.id);

        self.jobs.push(CronJob {
            config,
            schedule,
            last_run: None,
            run_count: 0,
        });

        self.save()?;
        Ok(())
    }

    /// Remove a job by ID.
    pub fn remove_job(&mut self, id: &str) -> Result<bool> {
        let before = self.jobs.len();
        self.jobs.retain(|j| j.config.id != id);
        let removed = self.jobs.len() < before;
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    /// List all jobs.
    pub fn list_jobs(&self) -> &[CronJob] {
        &self.jobs
    }

    /// Get a job by ID.
    pub fn get_job(&self, id: &str) -> Option<&CronJob> {
        self.jobs.iter().find(|j| j.config.id == id)
    }

    /// Check which jobs are due to run right now.
    ///
    /// Returns job configs for all enabled jobs whose next scheduled time
    /// is at or before the current time.
    pub fn tick(&mut self) -> Vec<JobConfig> {
        let now = Utc::now();
        let mut due = Vec::new();

        for job in &mut self.jobs {
            if !job.config.enabled {
                continue;
            }

            // Find the next scheduled time after last_run (or epoch if never run)
            let after = match job.last_run {
                Some(ts) => {
                    chrono::DateTime::from_timestamp(ts, 0).unwrap_or(chrono::DateTime::UNIX_EPOCH)
                }
                None => chrono::DateTime::UNIX_EPOCH,
            };

            if let Some(next) = job.schedule.after(&after).next()
                && next <= now
            {
                due.push(job.config.clone());
                job.last_run = Some(now.timestamp());
                job.run_count += 1;
            }
        }

        due
    }

    /// Record a job result.
    pub fn record_result(&self, result: &JobResult) -> Result<()> {
        // Append to results file (JSONL format)
        let line = serde_json::to_string(result).context("serialize result")?;

        use std::io::Write;
        if let Some(parent) = self.results_path.parent() {
            std::fs::create_dir_all(parent).context("create cron dir")?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.results_path)
            .context("open results file")?;
        writeln!(file, "{line}").context("write result")?;
        Ok(())
    }

    /// Load recent results (last N lines from results.json).
    pub fn recent_results(&self, limit: usize) -> Result<Vec<JobResult>> {
        if !self.results_path.exists() {
            return Ok(Vec::new());
        }

        let data = std::fs::read_to_string(&self.results_path).context("read results")?;
        let results: Vec<JobResult> = data
            .lines()
            .rev()
            .take(limit)
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();

        Ok(results)
    }

    /// Get the number of registered jobs.
    pub fn job_count(&self) -> usize {
        self.jobs.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Generates unique scheduler test directories for parallel test execution.
    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    /// Builds a unique temporary directory path for a scheduler test.
    fn unique_test_dir(prefix: &str) -> std::path::PathBuf {
        let id = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()))
    }

    /// Builds an isolated scheduler instance for tests.
    fn temp_scheduler() -> CronScheduler {
        let dir = unique_test_dir("synapse-cron-test");
        let _ = std::fs::remove_dir_all(&dir);
        CronScheduler::open(&dir).expect("open temp scheduler")
    }

    /// Verifies jobs can be added and listed from an isolated scheduler.
    #[test]
    fn add_and_list_jobs() {
        let mut sched = temp_scheduler();

        sched
            .add_job(JobConfig {
                id: "test-daily".into(),
                schedule: "0 0 9 * * *".into(), // 9am daily
                task: "run daily report".into(),
                cwd: None,
                model: None,
                provider: None,
                max_turns: 10,
                enabled: true,
            })
            .unwrap();

        assert_eq!(sched.job_count(), 1);
        assert_eq!(sched.list_jobs()[0].config.id, "test-daily");
    }

    /// Verifies removing jobs updates scheduler state and handles missing IDs.
    #[test]
    fn remove_job() {
        let mut sched = temp_scheduler();

        sched
            .add_job(JobConfig {
                id: "to-remove".into(),
                schedule: "0 */5 * * * *".into(),
                task: "test".into(),
                cwd: None,
                model: None,
                provider: None,
                max_turns: 5,
                enabled: true,
            })
            .unwrap();

        assert!(sched.remove_job("to-remove").unwrap());
        assert_eq!(sched.job_count(), 0);
        assert!(!sched.remove_job("nonexistent").unwrap());
    }

    /// Verifies persisted jobs survive scheduler reopen.
    #[test]
    fn persistence_roundtrip() {
        let dir = unique_test_dir("synapse-cron-persist");
        let _ = std::fs::remove_dir_all(&dir);

        {
            let mut sched = CronScheduler::open(&dir).unwrap();
            sched
                .add_job(JobConfig {
                    id: "persist-test".into(),
                    schedule: "0 0 * * * *".into(),
                    task: "hourly check".into(),
                    cwd: Some("/tmp".into()),
                    model: Some("claude-sonnet-4-20250514".into()),
                    provider: None,
                    max_turns: 8,
                    enabled: true,
                })
                .unwrap();
        }

        // Reopen and verify
        let sched = CronScheduler::open(&dir).unwrap();
        assert_eq!(sched.job_count(), 1);
        let job = sched.get_job("persist-test").unwrap();
        assert_eq!(job.config.task, "hourly check");
        assert_eq!(job.config.cwd.as_deref(), Some("/tmp"));
        assert_eq!(
            job.config.model.as_deref(),
            Some("claude-sonnet-4-20250514")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Verifies invalid cron expressions are rejected before persistence.
    #[test]
    fn invalid_cron_expression_rejected() {
        let mut sched = temp_scheduler();
        let result = sched.add_job(JobConfig {
            id: "bad".into(),
            schedule: "not a cron expression".into(),
            task: "test".into(),
            cwd: None,
            model: None,
            provider: None,
            max_turns: 5,
            enabled: true,
        });
        assert!(result.is_err());
    }

    /// Verifies ticking returns enabled jobs whose schedules are due.
    #[test]
    fn tick_finds_due_jobs() {
        let mut sched = temp_scheduler();

        // Add a job that runs every second (should always be due)
        sched
            .add_job(JobConfig {
                id: "every-second".into(),
                schedule: "* * * * * *".into(),
                task: "frequent task".into(),
                cwd: None,
                model: None,
                provider: None,
                max_turns: 5,
                enabled: true,
            })
            .unwrap();

        // Add a disabled job
        sched
            .add_job(JobConfig {
                id: "disabled".into(),
                schedule: "* * * * * *".into(),
                task: "disabled task".into(),
                cwd: None,
                model: None,
                provider: None,
                max_turns: 5,
                enabled: false,
            })
            .unwrap();

        let due = sched.tick();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "every-second");

        // Second tick should NOT return the same job (just ran)
        // (in practice it depends on timing, but the last_run was just set)
    }

    /// Verifies job results are appended and read back in recent-first order.
    #[test]
    fn record_and_read_results() {
        let dir = unique_test_dir("synapse-cron-results");
        let _ = std::fs::remove_dir_all(&dir);
        let sched = CronScheduler::open(&dir).unwrap();

        let result = JobResult {
            job_id: "test".into(),
            started_at: 1000,
            finished_at: 1010,
            output: "all good".into(),
            success: true,
        };
        sched.record_result(&result).unwrap();

        let results = sched.recent_results(10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].job_id, "test");
        assert!(results[0].success);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
