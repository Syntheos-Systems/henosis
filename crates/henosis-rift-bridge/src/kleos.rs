//! Kleos integration for bridge discussion context and consensus writeback.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use henosis_broca::{BrocaStore, LogAction};
use henosis_chiasm::{ChiasmStore, NewTask, Task, TaskFilter, TaskPatch, TaskStatus};
use henosis_memory_client::Client as MemoryClient;
use syntheos_contracts::{PrincipalId, TaskId, TenantId};

use crate::error::BridgeError;

/// Small memory hit surface the bridge needs from Kleos search.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KleosMemoryHit {
    /// Memory identifier.
    pub id: i64,
    /// Memory content text.
    pub content: String,
    /// Optional source marker.
    pub source: Option<String>,
}

/// Small task surface the bridge needs from Chiasm.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct KleosTaskSummary {
    /// Task identifier.
    pub id: i64,
    /// Assigned agent.
    pub agent: String,
    /// Human-readable task title.
    pub title: String,
    /// Current task status.
    pub status: String,
    /// Optional progress summary.
    pub summary: Option<String>,
}

/// Response envelope returned by Kleos memory search.
#[derive(Debug, Deserialize)]
struct KleosSearchResponse {
    /// Ranked memory results.
    #[serde(default)]
    results: Vec<KleosMemoryHit>,
}

/// Response envelope returned by Chiasm task listing.
#[derive(Debug, Deserialize)]
struct KleosTasksResponse {
    /// Matching task rows.
    #[serde(default)]
    tasks: Vec<KleosTaskSummary>,
}

/// Transport-agnostic Kleos operations needed by the bridge discussion loop.
#[async_trait]
pub trait KleosClient: Send + Sync {
    /// Search memory relevant to the current room discussion.
    async fn search_memories(
        &self,
        project: &str,
        channel: &str,
        recent_messages: &[(String, String)],
        limit: usize,
    ) -> Result<Vec<String>, BridgeError>;

    /// Load a compact set of active tasks for the current project.
    async fn active_tasks_summary(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Option<String>, BridgeError>;

    /// Report a coarse bridge lifecycle event into Kleos activity fan-out.
    async fn report_activity(
        &self,
        project: &str,
        agent: &str,
        action: &str,
        summary: &str,
        metadata: serde_json::Value,
    ) -> Result<(), BridgeError>;

    /// Store a compact consensus memory.
    async fn store_consensus_memory(
        &self,
        project: &str,
        content: &str,
        tags: &[String],
    ) -> Result<(), BridgeError>;

    /// Create a draft Chiasm task from actionable consensus.
    async fn create_draft_task(
        &self,
        project: &str,
        agent: &str,
        title: &str,
        summary: &str,
    ) -> Result<(), BridgeError>;

    /// Create a Chiasm task already claimed by the agent for execution, and
    /// return its task id.
    async fn create_execution_task(
        &self,
        project: &str,
        agent: &str,
        title: &str,
        description: &str,
    ) -> Result<String, BridgeError>;

    /// Update the status of a Chiasm task (e.g., "active", "completed",
    /// "blocked") with a short note.
    async fn update_task_status(
        &self,
        task_id: &str,
        status: &str,
        note: &str,
    ) -> Result<(), BridgeError>;
}

/// Real Kleos client backed by the local Kleos HTTP API.
pub struct HttpKleosClient {
    /// Underlying HTTP client.
    client: Client,
    /// Base URL for Kleos.
    base_url: String,
    /// Bearer API key for Kleos.
    api_key: String,
}

/// Implements construction and HTTP helpers for the real Kleos client.
impl HttpKleosClient {
    /// Build the client from standard Kleos environment variables.
    pub fn from_env() -> Result<Self, BridgeError> {
        let base_url =
            std::env::var("KLEOS_URL").unwrap_or_else(|_| "http://127.0.0.1:4200".to_string());
        let api_key = std::env::var("KLEOS_API_KEY")
            .or_else(|_| std::env::var("KLEOS_KEY"))
            .map_err(|_| BridgeError::Config("missing KLEOS_API_KEY or KLEOS_KEY".into()))?;

        Ok(Self {
            client: Client::new(),
            base_url,
            api_key,
        })
    }

    /// Build a full endpoint URL from a route path.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    /// Attach bearer auth to a request builder.
    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder.bearer_auth(&self.api_key)
    }

    /// Parse a Kleos HTTP response as JSON after preserving error bodies.
    async fn parse_json<T: DeserializeOwned>(
        &self,
        route: &str,
        response: reqwest::Response,
    ) -> Result<T, BridgeError> {
        let status = response.status();
        let body = response.text().await?;

        if !status.is_success() {
            return Err(BridgeError::Kleos(format!(
                "{route} failed ({status}): {body}"
            )));
        }

        serde_json::from_str(&body).map_err(|e| {
            BridgeError::Kleos(format!("{route} returned invalid JSON: {e}; body: {body}"))
        })
    }
}

/// Implements the bridge-facing Kleos operations over HTTP.
#[async_trait]
impl KleosClient for HttpKleosClient {
    /// Search memory relevant to the current room discussion.
    async fn search_memories(
        &self,
        project: &str,
        channel: &str,
        recent_messages: &[(String, String)],
        limit: usize,
    ) -> Result<Vec<String>, BridgeError> {
        let recent_refs: Vec<(&str, &str)> = recent_messages
            .iter()
            .map(|(author, text)| (author.as_str(), text.as_str()))
            .collect();
        let query = format!(
            "project:{} {}",
            project,
            build_memory_query(channel, &recent_refs)
        );

        let response = self
            .authed(
                self.client
                    .post(self.endpoint("/search"))
                    .json(&json!({ "query": query, "limit": limit })),
            )
            .send()
            .await?;
        let search: KleosSearchResponse = self.parse_json("/search", response).await?;

        Ok(search
            .results
            .into_iter()
            .map(|hit| {
                format!(
                    "[memory:{} source={}] {}",
                    hit.id,
                    hit.source.as_deref().unwrap_or("unknown"),
                    hit.content
                )
            })
            .collect())
    }

    /// Load a compact set of active tasks for the current project.
    async fn active_tasks_summary(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Option<String>, BridgeError> {
        let limit_text = limit.to_string();
        let response = self
            .authed(self.client.get(self.endpoint("/chiasm/tasks")).query(&[
                ("project", project),
                ("status", "active"),
                ("limit", limit_text.as_str()),
            ]))
            .send()
            .await?;
        let tasks: KleosTasksResponse = self.parse_json("/chiasm/tasks", response).await?;

        Ok(summarize_active_tasks(&tasks.tasks))
    }

    /// Report a coarse bridge lifecycle event into Kleos activity fan-out.
    async fn report_activity(
        &self,
        project: &str,
        agent: &str,
        action: &str,
        summary: &str,
        metadata: serde_json::Value,
    ) -> Result<(), BridgeError> {
        let response = self
            .authed(self.client.post(self.endpoint("/activity")).json(&json!({
                "project": project,
                "agent": agent,
                "action": action,
                "summary": summary,
                "metadata": metadata,
            })))
            .send()
            .await?;
        let _: serde_json::Value = self.parse_json("/activity", response).await?;
        Ok(())
    }

    /// Store a compact consensus memory.
    async fn store_consensus_memory(
        &self,
        project: &str,
        content: &str,
        tags: &[String],
    ) -> Result<(), BridgeError> {
        let mut all_tags = tags.to_vec();
        let project_tag = format!("project:{project}");
        if !all_tags.iter().any(|tag| tag == &project_tag) {
            all_tags.push(project_tag);
        }

        let response = self
            .authed(self.client.post(self.endpoint("/store")).json(&json!({
                "content": content,
                "category": "decision",
                "source": "rift-bridge",
                "importance": 6,
                "tags": all_tags,
            })))
            .send()
            .await?;
        let _: serde_json::Value = self.parse_json("/store", response).await?;
        Ok(())
    }

    /// Create a draft Chiasm task from actionable consensus.
    async fn create_draft_task(
        &self,
        project: &str,
        agent: &str,
        title: &str,
        summary: &str,
    ) -> Result<(), BridgeError> {
        let response = self
            .authed(
                self.client
                    .post(self.endpoint("/chiasm/tasks"))
                    .json(&json!({
                        "project": project,
                        "agent": agent,
                        "title": title,
                        "summary": summary,
                        "status": "blocked_on_human",
                        "output_format": "raw",
                    })),
            )
            .send()
            .await?;
        let _: serde_json::Value = self.parse_json("/chiasm/tasks", response).await?;
        Ok(())
    }

    /// Create a claimed Chiasm task and return the id from the response.
    async fn create_execution_task(
        &self,
        project: &str,
        agent: &str,
        title: &str,
        description: &str,
    ) -> Result<String, BridgeError> {
        let response = self
            .authed(
                self.client
                    .post(self.endpoint("/chiasm/tasks"))
                    .json(&json!({
                        "project": project,
                        "agent": agent,
                        "title": title,
                        "summary": description,
                        "status": "active",
                        "output_format": "raw",
                    })),
            )
            .send()
            .await?;
        let value: serde_json::Value = self.parse_json("/chiasm/tasks", response).await?;
        let id = value
            .get("id")
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .ok_or_else(|| BridgeError::Kleos("chiasm task create response missing id".into()))?;
        Ok(id)
    }

    /// Patch the task status via the Chiasm task endpoint.
    async fn update_task_status(
        &self,
        task_id: &str,
        status: &str,
        note: &str,
    ) -> Result<(), BridgeError> {
        let response = self
            .authed(
                self.client
                    .patch(self.endpoint(&format!("/chiasm/tasks/{task_id}")))
                    .json(&json!({
                        "status": status,
                        "note": note,
                    })),
            )
            .send()
            .await?;
        let _: serde_json::Value = self.parse_json("/chiasm/tasks/:id", response).await?;
        Ok(())
    }
}

/// In-process Kleos client: backs the bridge's [`KleosClient`] trait with the
/// henosis kernel stores instead of HTTP-to-Kleos.
///
/// Chiasm task operations and Broca activity run fully in-process against
/// `ChiasmStore`/`BrocaStore`. Memory operations route to upstream Kleos via the
/// workspace memory client, because no in-process vector store exists in the
/// workspace yet. This is the in-Henosis counterpart to [`HttpKleosClient`],
/// which stays the standalone-bridge path -- the same trait, two deployments
/// (mirroring the standalone/Henosis split of the Synapse PistisGate authority).
pub struct InProcessKleosClient {
    /// In-process Chiasm task store.
    chiasm: Arc<ChiasmStore>,
    /// In-process Broca action/narration store.
    broca: Arc<BrocaStore>,
    /// Memory client to upstream Kleos (generic signed HTTP).
    memory: Arc<MemoryClient>,
    /// Tenant all bridge kernel writes belong to.
    tenant: TenantId,
    /// The bridge service principal: owner/scope for all bridge Chiasm
    /// bookkeeping, so `active_tasks_summary` (a principal-scoped list) sees the
    /// tasks the bridge created. Per-agent identity is carried as the task
    /// assignee via [`crate::identity::principal_for_agent`].
    principal: PrincipalId,
}

/// Construction for the in-process Kleos client.
impl InProcessKleosClient {
    /// Build from injected kernel store handles, a tenant, and the bridge
    /// service principal.
    pub fn new(
        chiasm: Arc<ChiasmStore>,
        broca: Arc<BrocaStore>,
        memory: Arc<MemoryClient>,
        tenant: TenantId,
        principal: PrincipalId,
    ) -> Self {
        Self {
            chiasm,
            broca,
            memory,
            tenant,
            principal,
        }
    }
}

/// Collapse in-process Chiasm task rows into one compact executor-facing summary.
///
/// Mirrors [`summarize_active_tasks`] but over typed [`Task`] rows. The id is the
/// task UUID (`KleosTaskSummary` carried an i64; `Task.id` is a `TaskId`). The
/// agent name is omitted -- it is not recoverable from the assignee principal
/// (the bridge resolves names to principals one-way), a documented fidelity
/// difference from the HTTP path.
fn summarize_tasks(tasks: &[Task]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }
    Some(
        tasks
            .iter()
            .take(5)
            .map(|task| match &task.summary {
                Some(summary) => format!(
                    "#{} {} -- {} ({})",
                    task.id,
                    task.title,
                    summary,
                    task.status.as_str()
                ),
                None => format!("#{} {} ({})", task.id, task.title, task.status.as_str()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Implements the bridge-facing Kleos operations in-process over kernel stores.
#[async_trait]
impl KleosClient for InProcessKleosClient {
    /// Search memory via the upstream Kleos memory client (no in-process store).
    async fn search_memories(
        &self,
        project: &str,
        channel: &str,
        recent_messages: &[(String, String)],
        limit: usize,
    ) -> Result<Vec<String>, BridgeError> {
        let recent_refs: Vec<(&str, &str)> = recent_messages
            .iter()
            .map(|(author, text)| (author.as_str(), text.as_str()))
            .collect();
        let query = format!(
            "project:{} {}",
            project,
            build_memory_query(channel, &recent_refs)
        );
        let resp = self
            .memory
            .post("/search", json!({ "query": query, "limit": limit }))
            .await
            .map_err(BridgeError::Kleos)?;
        let results = resp
            .get("results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(results
            .iter()
            .map(|hit| {
                let id = hit
                    .get("id")
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_else(|| "?".to_string());
                let source = hit.get("source").and_then(Value::as_str).unwrap_or("unknown");
                let content = hit.get("content").and_then(Value::as_str).unwrap_or("");
                format!("[memory:{id} source={source}] {content}")
            })
            .collect())
    }

    /// List the project's active tasks in-process via `ChiasmStore`.
    async fn active_tasks_summary(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Option<String>, BridgeError> {
        let filter = TaskFilter {
            status: Some(TaskStatus::Active),
            project: Some(project.to_string()),
            limit: Some(limit),
            offset: None,
        };
        let tasks = self
            .chiasm
            .list(self.principal, filter)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(summarize_tasks(&tasks))
    }

    /// Log a bridge lifecycle event in-process via `BrocaStore`.
    async fn report_activity(
        &self,
        project: &str,
        agent: &str,
        action: &str,
        summary: &str,
        metadata: serde_json::Value,
    ) -> Result<(), BridgeError> {
        // Broca's LogAction has no project/agent slots, so pack them into the
        // payload (which must be a JSON object) alongside the caller metadata.
        let mut payload = serde_json::Map::new();
        payload.insert("project".to_string(), json!(project));
        payload.insert("agent".to_string(), json!(agent));
        payload.insert("summary".to_string(), json!(summary));
        match metadata {
            Value::Object(map) => {
                for (k, v) in map {
                    payload.insert(k, v);
                }
            }
            Value::Null => {}
            // A non-object metadata value cannot be merged into the object; nest
            // it under a key so the payload stays a valid object for Broca.
            other => {
                payload.insert("metadata".to_string(), other);
            }
        }
        let req = LogAction {
            tenant: self.tenant,
            principal_id: crate::identity::principal_for_agent(agent),
            service: Some("rift-bridge".to_string()),
            action: action.to_string(),
            payload: Some(Value::Object(payload)),
            narrative: None,
        };
        self.broca
            .log(req)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(())
    }

    /// Store a consensus memory via the upstream Kleos memory client.
    ///
    /// Preserves `category: decision` / `importance: 6` (the gateway client
    /// would flatten these to general/5) by posting the full body directly.
    async fn store_consensus_memory(
        &self,
        project: &str,
        content: &str,
        tags: &[String],
    ) -> Result<(), BridgeError> {
        let mut all_tags = tags.to_vec();
        let project_tag = format!("project:{project}");
        if !all_tags.iter().any(|tag| tag == &project_tag) {
            all_tags.push(project_tag);
        }
        self.memory
            .post(
                "/store",
                json!({
                    "content": content,
                    "category": "decision",
                    "source": "rift-bridge",
                    "importance": 6,
                    "tags": all_tags,
                }),
            )
            .await
            .map_err(BridgeError::Kleos)?;
        Ok(())
    }

    /// Create a human-blocked draft task in-process via `ChiasmStore`.
    async fn create_draft_task(
        &self,
        project: &str,
        agent: &str,
        title: &str,
        summary: &str,
    ) -> Result<(), BridgeError> {
        let new = NewTask {
            tenant: self.tenant,
            principal_id: self.principal,
            project: project.to_string(),
            title: title.to_string(),
            status: Some(TaskStatus::BlockedOnHuman),
            summary: Some(summary.to_string()),
            expected_output: None,
            output_format: Some("raw".to_string()),
            assignee: Some(crate::identity::principal_for_agent(agent)),
            heartbeat_interval_secs: None,
        };
        self.chiasm
            .create(new)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(())
    }

    /// Create an active (claimed) task in-process and return its id string.
    async fn create_execution_task(
        &self,
        project: &str,
        agent: &str,
        title: &str,
        description: &str,
    ) -> Result<String, BridgeError> {
        let new = NewTask {
            tenant: self.tenant,
            principal_id: self.principal,
            project: project.to_string(),
            title: title.to_string(),
            status: Some(TaskStatus::Active),
            summary: Some(description.to_string()),
            expected_output: None,
            output_format: Some("raw".to_string()),
            assignee: Some(crate::identity::principal_for_agent(agent)),
            heartbeat_interval_secs: None,
        };
        let task = self
            .chiasm
            .create(new)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(task.id.to_string())
    }

    /// Patch a task's status in-process via `ChiasmStore`.
    ///
    /// `note` maps to the task `summary` (Chiasm has no separate note field);
    /// this clobbers any prior progress summary -- a documented fidelity caveat
    /// matching the HTTP path's `note` field semantics.
    async fn update_task_status(
        &self,
        task_id: &str,
        status: &str,
        note: &str,
    ) -> Result<(), BridgeError> {
        let id = task_id
            .parse::<TaskId>()
            .map_err(|e| BridgeError::Kleos(format!("invalid task id {task_id}: {e}")))?;
        let status = TaskStatus::parse(status).map_err(|e| BridgeError::Kleos(e.to_string()))?;
        let patch = TaskPatch {
            title: None,
            status: Some(status),
            summary: Some(note.to_string()),
            assignee: None,
        };
        self.chiasm
            .update(self.principal, id, patch)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(())
    }
}

/// Build a compact memory query from channel and recent conversation.
pub fn build_memory_query(channel: &str, recent_messages: &[(&str, &str)]) -> String {
    let joined = recent_messages
        .iter()
        .rev()
        .take(6)
        .map(|(_, text)| *text)
        .collect::<Vec<_>>()
        .join(" ");
    format!("#{} {}", channel, joined)
}

/// Collapse active task rows into one compact executor-facing summary string.
pub fn summarize_active_tasks(tasks: &[KleosTaskSummary]) -> Option<String> {
    if tasks.is_empty() {
        return None;
    }

    Some(
        tasks
            .iter()
            .take(5)
            .map(|task| match &task.summary {
                Some(summary) => format!(
                    "#{} [{}] {} -- {} ({})",
                    task.id, task.agent, task.title, summary, task.status
                ),
                None => format!(
                    "#{} [{}] {} ({})",
                    task.id, task.agent, task.title, task.status
                ),
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
/// Tests for Kleos context helper behavior.
mod tests {
    use super::{build_memory_query, summarize_active_tasks, KleosTaskSummary};

    /// Verifies the memory query keeps the channel and recent human context.
    #[test]
    fn test_build_memory_query_uses_channel_and_recent_text() {
        let query = build_memory_query(
            "general",
            &[
                ("Alice", "We should wire shared memory into this room."),
                ("Bob", "Active tasks should show up in context too."),
            ],
        );

        assert!(query.contains("#general"));
        assert!(query.contains("shared memory"));
        assert!(query.contains("Active tasks"));
    }

    /// Verifies task summaries stay compact and include only the most useful fields.
    #[test]
    fn test_summarize_active_tasks_limits_and_formats() {
        let summary = summarize_active_tasks(&[
            KleosTaskSummary {
                id: 11,
                agent: "architect".into(),
                title: "Design Kleos integration".into(),
                status: "active".into(),
                summary: Some("Define the bridge-side contract".into()),
            },
            KleosTaskSummary {
                id: 12,
                agent: "builder".into(),
                title: "Patch context assembly".into(),
                status: "blocked".into(),
                summary: None,
            },
        ])
        .expect("tasks should produce a summary");

        assert!(summary.contains("#11"));
        assert!(summary.contains("architect"));
        assert!(summary.contains("blocked"));
        assert!(summary.contains("Design Kleos integration"));
    }
}

#[cfg(test)]
/// Tests for the in-process Kleos client over in-memory kernel stores.
///
/// The Chiasm/Broca paths run fully in-process and are exercised here. The two
/// memory methods route to upstream Kleos over HTTP, so they are not covered by
/// these store-only tests; they share the `build_memory_query` helper which is
/// tested above.
mod in_process_tests {
    use super::*;
    use henosis_broca::BrocaStore;
    use henosis_chiasm::ChiasmStore;
    use henosis_memory_client::Client as MemoryClient;
    use std::sync::Arc;
    use syntheos_axon::AxonBus;
    use syntheos_contracts::{PrincipalId, TenantId};

    /// Build an in-process client over fresh in-memory stores. The memory client
    /// points at an unused address; the tests below never call a memory method.
    fn client() -> (InProcessKleosClient, TenantId, PrincipalId) {
        let bus = Arc::new(AxonBus::new());
        let chiasm = Arc::new(ChiasmStore::open_in_memory(bus.clone()).unwrap());
        let broca = Arc::new(BrocaStore::open_in_memory(bus).unwrap());
        let memory = Arc::new(MemoryClient::new("http://127.0.0.1:1".to_string(), None, None));
        let tenant = TenantId::new();
        let principal = PrincipalId::new();
        (
            InProcessKleosClient::new(chiasm, broca, memory, tenant, principal),
            tenant,
            principal,
        )
    }

    /// An execution task is created and then surfaces in the active-task summary.
    #[tokio::test]
    async fn execution_task_round_trips_into_summary() {
        let (kleos, _, _) = client();
        let id = kleos
            .create_execution_task("proj", "architect", "Wire the gateway", "do the thing")
            .await
            .expect("create execution task");
        // The returned id is a parseable task UUID.
        assert!(!id.is_empty());

        let summary = kleos
            .active_tasks_summary("proj", 10)
            .await
            .expect("summary")
            .expect("one active task");
        assert!(summary.contains("Wire the gateway"), "summary: {summary}");
        assert!(summary.contains("active"), "summary: {summary}");
    }

    /// A draft task is created blocked-on-human and is NOT listed as active.
    #[tokio::test]
    async fn draft_task_is_not_active() {
        let (kleos, _, _) = client();
        kleos
            .create_draft_task("proj", "architect", "Maybe later", "needs human ok")
            .await
            .expect("create draft task");
        let summary = kleos.active_tasks_summary("proj", 10).await.expect("summary");
        assert!(summary.is_none(), "blocked_on_human task must not be active");
    }

    /// Updating an execution task to completed removes it from the active list.
    #[tokio::test]
    async fn update_status_moves_task_out_of_active() {
        let (kleos, _, _) = client();
        let id = kleos
            .create_execution_task("proj", "architect", "Ship it", "in progress")
            .await
            .expect("create");
        kleos
            .update_task_status(&id, "completed", "done and verified")
            .await
            .expect("update status");
        let summary = kleos.active_tasks_summary("proj", 10).await.expect("summary");
        assert!(summary.is_none(), "completed task must not be active");
    }

    /// A bad status token is a clean error, not a panic.
    #[tokio::test]
    async fn update_status_rejects_unknown_status() {
        let (kleos, _, _) = client();
        let id = kleos
            .create_execution_task("proj", "architect", "T", "d")
            .await
            .expect("create");
        let err = kleos.update_task_status(&id, "teleported", "n").await;
        assert!(err.is_err(), "unknown status must error");
    }

    /// report_activity logs to Broca in-process without error.
    #[tokio::test]
    async fn report_activity_logs() {
        let (kleos, _, _) = client();
        kleos
            .report_activity(
                "proj",
                "architect",
                "bridge.started",
                "room opened",
                serde_json::json!({ "room": "!abc" }),
            )
            .await
            .expect("report activity");
    }
}
