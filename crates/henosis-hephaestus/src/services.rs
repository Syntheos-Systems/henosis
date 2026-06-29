//! Best-effort coordination clients: Kleos memory, Chiasm tasks, Axon events,
//! and `cred` credential lookups. Every call here is allowed to fail without
//! aborting the calling task: failures log a warning and return a benign
//! default (None, empty vec, or silently no-op). This isolation keeps the
//! orchestrator and provider modules from having any knowledge of how the
//! surrounding service mesh is reached.
//!
//! These were originally bundled inside `Clients` in clients.rs. Extracted as
//! part of the Phase 1 refactor so the orchestrator can depend on a small,
//! provider-agnostic surface.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::checkpoint::Checkpoint;
use crate::config::Config;
use crate::tasks::TaskRecord;

/// Bundle of coordination-service clients shared across the orchestrator and
/// task layer. Holds a reqwest client and a borrowed-by-value `Config`.
#[derive(Clone)]
pub struct Services {
    /// Shared HTTP client; all coordination calls use this pool.
    http: Client,
    /// Runtime configuration (URLs, token slots, timeouts).
    cfg: Config,
}

/// Implementation block for `Services`. Each method is best-effort: a
/// failure logs a warning and returns a benign default rather than
/// propagating the error to the caller.
impl Services {
    /// Construct a new Services bundle. The reqwest client is shared with the
    /// rest of the application so connection pools stay unified.
    pub fn new(http: Client, cfg: Config) -> Self {
        Self { http, cfg }
    }

    /// Access the underlying config -- exposed because the orchestrator needs
    /// the same Config values (sandbox memory, timeouts, etc.) and we route
    /// those through Services rather than threading Config separately.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Reuse the shared HTTP client for callers that need to make additional
    /// requests (provider implementations, in particular) without spinning up
    /// a second connection pool.
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Fetch a secret via the `cred` CLI. Returns None if the binary is
    /// missing, prompts interactively (e.g. YubiKey), or returns a non-zero
    /// exit code. Hard 3s ceiling so a locked credential store cannot stall
    /// the task loop. When `cfg.cred_enabled` is false (e.g. integration
    /// tests) this is a no-op returning None immediately so cred subprocesses
    /// never spawn.
    pub async fn cred_get(&self, slot: &str) -> Option<String> {
        if !self.cfg.cred_enabled {
            return None;
        }
        let (ns, key) = slot.split_once('/').unwrap_or((slot, ""));
        let fut = Command::new("cred")
            .args(["get", ns, key, "--raw"])
            .stdin(std::process::Stdio::null())
            .output();
        let output = tokio::time::timeout(Duration::from_secs(3), fut)
            .await
            .ok()?
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let s = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if s.is_empty() { None } else { Some(s) }
    }

    /// Mirror a TaskRecord to Kleos for crash recovery. Best-effort: warns on
    /// failure and never blocks the caller.
    pub async fn kleos_store_task(&self, task: &TaskRecord) {
        let content = match serde_json::to_string(task) {
            Ok(s) => s,
            Err(e) => {
                warn!(task_id = %task.id, error = %e, "kleos_store_task: serialize failed");
                return;
            }
        };
        // Use serde to get the lowercase status string from the enum.
        let status_str = serde_json::to_value(task.status)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unknown".to_string());

        let token = self.cred_get(&self.cfg.kleos_token_slot).await;
        let url = format!("{}/store", self.cfg.kleos_url.trim_end_matches('/'));
        let body = json!({
            "content": content,
            "category": "hephaestus_task",
            "source": "hephaestus",
            "tags": ["task", status_str],
            "importance": 5,
        });
        let mut req = self
            .http
            .post(&url)
            .timeout(self.cfg.http_timeout)
            .json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                debug!(task_id = %task.id, "kleos task mirror ok")
            }
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(task_id = %task.id, status = %s, body = %b, "kleos task mirror non-2xx");
            }
            Err(e) => warn!(task_id = %task.id, error = %e, "kleos task mirror failed"),
        }
    }

    /// Search Kleos for tasks that may need to be resumed (running, paused, or
    /// accepted). Called once at startup. Best-effort: returns empty vec on
    /// any error path.
    pub async fn kleos_recover_tasks(&self) -> Vec<Value> {
        let token = self.cred_get(&self.cfg.kleos_token_slot).await;
        let url = format!(
            "{}/search?q=hephaestus_task&limit=200",
            self.cfg.kleos_url.trim_end_matches('/')
        );
        let mut req = self.http.get(&url).timeout(self.cfg.http_timeout);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.unwrap_or(Value::Array(vec![]));
                v.as_array().cloned().unwrap_or_default()
            }
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(status = %s, body = %b, "kleos recover non-2xx");
                vec![]
            }
            Err(e) => {
                warn!(error = %e, "kleos recover failed");
                vec![]
            }
        }
    }

    /// Persist a per-turn checkpoint to Kleos. Tagged with the unique
    /// `checkpoint:<task_id>` so `kleos_load_latest_checkpoint` can find it
    /// without scanning every memory.
    pub async fn kleos_store_checkpoint(&self, cp: &Checkpoint) {
        let content = match serde_json::to_string(cp) {
            Ok(s) => s,
            Err(e) => {
                warn!(task_id = %cp.task_id, error = %e, "checkpoint serialize failed");
                return;
            }
        };
        let token = self.cred_get(&self.cfg.kleos_token_slot).await;
        let url = format!("{}/store", self.cfg.kleos_url.trim_end_matches('/'));
        let body = json!({
            "content": content,
            "category": "hephaestus_checkpoint",
            "source": "hephaestus",
            "tags": ["checkpoint", Checkpoint::unique_tag(&cp.task_id)],
            "importance": 4,
        });
        let mut req = self
            .http
            .post(&url)
            .timeout(self.cfg.http_timeout)
            .json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                debug!(task_id = %cp.task_id, step = cp.step, "checkpoint stored")
            }
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(task_id = %cp.task_id, status = %s, body = %b, "checkpoint store non-2xx");
            }
            Err(e) => warn!(task_id = %cp.task_id, error = %e, "checkpoint store failed"),
        }
    }

    /// Load the most recent checkpoint for a task by tag, returning None if no
    /// checkpoint has been written yet.
    pub async fn kleos_load_latest_checkpoint(&self, task_id: &str) -> Option<Checkpoint> {
        let token = self.cred_get(&self.cfg.kleos_token_slot).await;
        let tag = Checkpoint::unique_tag(task_id);
        let url = format!("{}/search", self.cfg.kleos_url.trim_end_matches('/'));
        let mut req = self
            .http
            .get(&url)
            .timeout(self.cfg.http_timeout)
            .query(&[("q", tag.as_str()), ("limit", "50")]);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let arr: Vec<Value> = match req.send().await {
            Ok(r) if r.status().is_success() => r
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v.as_array().cloned())
                .unwrap_or_default(),
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(task_id, status = %s, body = %b, "checkpoint load non-2xx");
                return None;
            }
            Err(e) => {
                warn!(task_id, error = %e, "checkpoint load failed");
                return None;
            }
        };

        // Each Kleos memory wraps the serialized Checkpoint in `content`. Pick
        // the latest by created_at so checkpoint ordering is monotonic even
        // when search returns memories out of order.
        let mut best: Option<Checkpoint> = None;
        for mem in arr {
            let content = mem.get("content").and_then(|c| c.as_str())?;
            let parsed: Checkpoint = match serde_json::from_str(content) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if parsed.task_id != task_id {
                continue;
            }
            best = match best {
                None => Some(parsed),
                Some(prev) if parsed.created_at > prev.created_at => Some(parsed),
                Some(prev) => Some(prev),
            };
        }
        best
    }

    /// Store the final user/assistant thread as an `agent_thread` Kleos
    /// memory. Best-effort; warnings logged on failure.
    pub async fn kleos_store_thread(&self, session_id: &str, content: &str) {
        let token = self.cred_get(&self.cfg.kleos_token_slot).await;
        let url = format!("{}/store", self.cfg.kleos_url.trim_end_matches('/'));
        let body = json!({
            "content": content,
            "category": "agent_thread",
            "source": "hephaestus",
            "session_id": session_id,
        });
        let mut req = self
            .http
            .post(&url)
            .timeout(self.cfg.http_timeout)
            .json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => debug!(%url, "kleos store ok"),
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(status = %s, body = %b, "kleos store non-2xx");
            }
            Err(e) => warn!(error = %e, "kleos store failed"),
        }
    }

    /// Create a Chiasm coordination task. Returns the Chiasm task id on
    /// success, or None on any failure. Best-effort.
    pub async fn chiasm_create_task(
        &self,
        title: &str,
        summary: &str,
        project: &str,
    ) -> Option<i64> {
        let url = format!("{}/tasks", self.cfg.chiasm_url.trim_end_matches('/'));
        let body = json!({
            "agent": self.cfg.chiasm_agent,
            "project": project,
            "title": title,
            "summary": summary,
        });
        let mut req = self
            .http
            .post(&url)
            .timeout(self.cfg.http_timeout)
            .json(&body);
        if let Some(slot) = &self.cfg.chiasm_token_slot
            && let Some(t) = self.cred_get(slot).await
        {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                let v: Value = r.json().await.ok()?;
                v.get("id").and_then(|i| i.as_i64())
            }
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(status = %s, body = %b, "chiasm create non-2xx");
                None
            }
            Err(e) => {
                warn!(error = %e, "chiasm create failed");
                None
            }
        }
    }

    /// Submit a final output to a Chiasm task. Best-effort.
    pub async fn chiasm_submit_output(&self, chiasm_id: i64, output: &str) {
        let url = format!(
            "{}/tasks/{}/output",
            self.cfg.chiasm_url.trim_end_matches('/'),
            chiasm_id
        );
        let body = json!({ "output": output });
        let mut req = self
            .http
            .post(&url)
            .timeout(self.cfg.http_timeout)
            .json(&body);
        if let Some(slot) = &self.cfg.chiasm_token_slot
            && let Some(t) = self.cred_get(slot).await
        {
            req = req.bearer_auth(t);
        }
        match req.send().await {
            Ok(r) if r.status().is_success() => debug!(%url, "chiasm output ok"),
            Ok(r) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(status = %s, body = %b, "chiasm output non-2xx");
            }
            Err(e) => warn!(error = %e, "chiasm output failed"),
        }
    }

    /// Publish an event to Axon. Hard 10s ceiling so a stalled Axon never
    /// pins a task. Best-effort with warning on every non-happy path.
    pub async fn axon_publish(&self, channel: &str, action: &str, payload: Value) {
        let url = format!("{}/axon/publish", self.cfg.axon_url.trim_end_matches('/'));
        let body = AxonPublishBody {
            channel: channel.to_string(),
            action: action.to_string(),
            payload: Some(payload),
            source: Some("hephaestus".to_string()),
            agent: Some(self.cfg.chiasm_agent.clone()),
        };
        let token = self.cred_get(&self.cfg.kleos_token_slot).await;
        let mut req = self
            .http
            .post(&url)
            .timeout(self.cfg.http_timeout)
            .json(&body);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        match tokio::time::timeout(Duration::from_secs(10), req.send()).await {
            Ok(Ok(r)) if r.status().is_success() => debug!(channel, action, "axon publish ok"),
            Ok(Ok(r)) => {
                let s = r.status();
                let b = r.text().await.unwrap_or_default();
                warn!(status = %s, body = %b, "axon publish non-2xx");
            }
            Ok(Err(e)) => warn!(error = %e, "axon publish failed"),
            Err(_) => warn!("axon publish timed out"),
        }
    }
}

/// Wire format for an Axon publish body. Mirrors the schema Axon expects on
/// POST /axon/publish.
#[derive(Debug, Serialize, Deserialize)]
struct AxonPublishBody {
    /// Event bus channel name (e.g. "hephaestus.tasks").
    channel: String,
    /// Action identifier (e.g. "task.started").
    action: String,
    /// Arbitrary event payload.
    payload: Option<Value>,
    /// Source service name, used for audit and routing.
    source: Option<String>,
    /// Agent name, forwarded from config.
    agent: Option<String>,
}
