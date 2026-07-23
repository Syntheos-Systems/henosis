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

/// One Axon event as returned by `GET /axon/events`. The activity fan-out
/// packs project and summary into the payload object rather than top-level
/// columns, so the payload is kept raw here and flattened by the Brief
/// conversion.
#[derive(Debug, Deserialize)]
struct KleosAxonEvent {
    /// Monotonic event id; the since_id cursor compares against it.
    id: i64,
    /// Routing channel derived from the action (alerts, tasks, quality, system).
    channel: String,
    /// Activity action string (e.g. task.completed, error.raised).
    action: String,
    /// Reporting agent name, when the publisher supplied one.
    #[serde(default)]
    agent: Option<String>,
    /// Publisher payload; activity fan-out stores {agent, action, summary, project?}.
    #[serde(default)]
    payload: serde_json::Value,
}

/// Response envelope returned by Axon event listing.
#[derive(Debug, Deserialize)]
struct KleosAxonEventsResponse {
    /// Matching events, oldest first within the queried window.
    #[serde(default)]
    events: Vec<KleosAxonEvent>,
}

/// One Loom workflow run as returned by `GET /loom/runs`.
#[derive(Debug, Deserialize)]
struct KleosLoomRun {
    /// Run id.
    id: i64,
    /// Owning workflow id.
    workflow_id: i64,
    /// Current run status string (vocabulary is open; treated opaquely).
    status: String,
    /// Failure detail when the run errored.
    #[serde(default)]
    error: Option<String>,
}

/// Response envelope returned by Loom run listing.
#[derive(Debug, Deserialize)]
struct KleosLoomRunsResponse {
    /// Matching run rows, most recent first.
    #[serde(default)]
    runs: Vec<KleosLoomRun>,
}

/// Compact Axon event view handed to stimulus sources.
#[derive(Debug, Clone)]
pub struct AxonEventBrief {
    /// Monotonic event id (cursor value).
    pub id: i64,
    /// Routing channel (alerts, tasks, quality, system).
    pub channel: String,
    /// Activity action string.
    pub action: String,
    /// Reporting agent name, when known.
    pub agent: Option<String>,
    /// Project the activity belongs to, when the payload carried one.
    pub project: Option<String>,
    /// Human-readable activity summary, when the payload carried one.
    pub summary: Option<String>,
}

/// Flatten a wire event into the Brief the stimulus source consumes.
impl From<KleosAxonEvent> for AxonEventBrief {
    /// Pull project and summary out of the activity payload object.
    fn from(event: KleosAxonEvent) -> Self {
        let project = event.payload["project"].as_str().map(str::to_string);
        let summary = event.payload["summary"].as_str().map(str::to_string);
        Self {
            id: event.id,
            channel: event.channel,
            action: event.action,
            agent: event.agent,
            project,
            summary,
        }
    }
}

/// Compact Loom run view handed to stimulus sources.
#[derive(Debug, Clone)]
pub struct LoomRunBrief {
    /// Run id.
    pub id: i64,
    /// Owning workflow id.
    pub workflow_id: i64,
    /// Current status string, treated opaquely.
    pub status: String,
    /// Failure detail when the run errored.
    pub error: Option<String>,
}

/// Narrow a wire run row into the Brief the stimulus source consumes.
impl From<KleosLoomRun> for LoomRunBrief {
    /// Carry the fields transition detection needs.
    fn from(run: KleosLoomRun) -> Self {
        Self {
            id: run.id,
            workflow_id: run.workflow_id,
            status: run.status,
            error: run.error,
        }
    }
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

    /// List Axon events newer than the given cursor id, oldest cursor-eligible
    /// first. Defaults to empty: the in-process AxonBus is pure pub/sub with
    /// no query surface, so only backends with a queryable event store (the
    /// HTTP path) override this; everyone else simply contributes no
    /// activity-event stimuli.
    async fn list_axon_events_since(
        &self,
        _since_id: Option<i64>,
        _limit: usize,
    ) -> Result<Vec<AxonEventBrief>, BridgeError> {
        Ok(Vec::new())
    }

    /// List recent Loom workflow runs. Defaults to empty: there is no
    /// in-process Loom store, so only the HTTP backend overrides this.
    async fn list_workflow_runs(&self, _limit: usize) -> Result<Vec<LoomRunBrief>, BridgeError> {
        Ok(Vec::new())
    }
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

    /// List Axon events past the cursor via `GET /axon/events`.
    async fn list_axon_events_since(
        &self,
        since_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<AxonEventBrief>, BridgeError> {
        let limit_text = limit.to_string();
        let mut query: Vec<(&str, String)> = vec![("limit", limit_text)];
        if let Some(id) = since_id {
            query.push(("since_id", id.to_string()));
        }
        let response = self
            .authed(self.client.get(self.endpoint("/axon/events")).query(&query))
            .send()
            .await?;
        let events: KleosAxonEventsResponse = self.parse_json("/axon/events", response).await?;
        Ok(events
            .events
            .into_iter()
            .map(AxonEventBrief::from)
            .collect())
    }

    /// List recent Loom workflow runs via `GET /loom/runs`.
    async fn list_workflow_runs(&self, limit: usize) -> Result<Vec<LoomRunBrief>, BridgeError> {
        let limit_text = limit.to_string();
        let response = self
            .authed(
                self.client
                    .get(self.endpoint("/loom/runs"))
                    .query(&[("limit", limit_text.as_str())]),
            )
            .send()
            .await?;
        let runs: KleosLoomRunsResponse = self.parse_json("/loom/runs", response).await?;
        Ok(runs.runs.into_iter().map(LoomRunBrief::from).collect())
    }
}

/// One memory hit, transport-agnostic: the fields the bridge renders into its
/// discussion context, normalized across the HTTP and cognition backends.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    /// Memory identifier (string, since the two backends carry different id types).
    pub id: String,
    /// Source marker (defaults to `unknown` when a backend omits it).
    pub source: String,
    /// Memory content text.
    pub content: String,
}

/// The bridge's memory-backend seam: the two memory operations the in-process
/// client needs, behind one trait so it can be backed either by upstream Kleos
/// over HTTP ([`HttpMemoryBackend`]) or by the in-process cognition store
/// ([`CognitionMemoryBackend`], `cognition` feature). Chiasm/Broca always run
/// in-process; memory can use either backend.
#[async_trait]
pub trait BridgeMemory: Send + Sync {
    /// Search memory for `query`, returning up to `limit` hits.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryRow>, BridgeError>;

    /// Store one consensus memory (`category: decision`, `importance: 6`,
    /// `source: rift-bridge`) with the given tags.
    async fn store(&self, content: &str, tags: &[String]) -> Result<(), BridgeError>;
}

/// Memory backend over upstream Kleos via the workspace memory client (generic
/// signed HTTP to `:4200`). The default backend, and the only one compiled into
/// the bridge when the `cognition` feature is off.
pub struct HttpMemoryBackend {
    /// Memory client to upstream Kleos.
    memory: Arc<MemoryClient>,
}

/// Construction for the HTTP memory backend.
impl HttpMemoryBackend {
    /// Wrap a memory client as the bridge's memory backend.
    pub fn new(memory: Arc<MemoryClient>) -> Self {
        Self { memory }
    }
}

/// Memory operations against upstream Kleos over HTTP.
#[async_trait]
impl BridgeMemory for HttpMemoryBackend {
    /// Search via the upstream Kleos `/search` endpoint.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryRow>, BridgeError> {
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
                let source = hit
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let content = hit
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                MemoryRow {
                    id,
                    source,
                    content,
                }
            })
            .collect())
    }

    /// Store via the upstream Kleos `/store` endpoint.
    ///
    /// Posts the full body directly (rather than via a flattening client helper)
    /// to preserve `category: decision` / `importance: 6`.
    async fn store(&self, content: &str, tags: &[String]) -> Result<(), BridgeError> {
        self.memory
            .post(
                "/store",
                json!({
                    "content": content,
                    "category": "decision",
                    "source": "rift-bridge",
                    "importance": 6,
                    "tags": tags,
                }),
            )
            .await
            .map_err(BridgeError::Kleos)?;
        Ok(())
    }
}

/// Memory backend over the in-process cognition store (vendored kleos-lib via
/// the `henosis-cognition` facade). This path uses no HTTP or `:4200`: memory
/// storage and FTS search run against a local kleos-lib
/// `Database` in the bridge process. Gated on the `cognition` feature so the
/// default bridge build never compiles the heavy ML stack.
#[cfg(feature = "cognition")]
pub struct CognitionMemoryBackend {
    /// The in-process cognitive core facade.
    cognition: Arc<henosis_cognition::Cognition>,
}

#[cfg(feature = "cognition")]
/// Construction for the cognition memory backend.
impl CognitionMemoryBackend {
    /// Wrap a cognition handle as the bridge's memory backend.
    pub fn new(cognition: Arc<henosis_cognition::Cognition>) -> Self {
        Self { cognition }
    }
}

/// Memory operations against the in-process cognition store.
#[cfg(feature = "cognition")]
#[async_trait]
impl BridgeMemory for CognitionMemoryBackend {
    /// Search the in-process cognition store (FTS when no embedder is attached).
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryRow>, BridgeError> {
        let req = henosis_cognition::SearchRequest {
            query: query.to_string(),
            limit: Some(limit),
            ..Default::default()
        };
        let hits = self
            .cognition
            .memory_search(req)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(hits
            .iter()
            .map(|hit| MemoryRow {
                id: hit.memory.id.to_string(),
                source: hit.memory.source.clone(),
                content: hit.memory.content.clone(),
            })
            .collect())
    }

    /// Store a consensus memory in the in-process cognition store.
    async fn store(&self, content: &str, tags: &[String]) -> Result<(), BridgeError> {
        let req = henosis_cognition::StoreRequest {
            content: content.to_string(),
            source: "rift-bridge".to_string(),
            category: "decision".to_string(),
            importance: 6,
            tags: Some(tags.to_vec()),
            ..Default::default()
        };
        self.cognition
            .memory_store(req)
            .await
            .map_err(|e| BridgeError::Kleos(e.to_string()))?;
        Ok(())
    }
}

/// In-process Kleos client: backs the bridge's [`KleosClient`] trait with the
/// henosis kernel stores instead of HTTP-to-Kleos.
///
/// Chiasm task operations and Broca activity run fully in-process against
/// `ChiasmStore`/`BrocaStore`. Memory operations go through the [`BridgeMemory`]
/// seam: upstream Kleos over HTTP by default, or the in-process cognition store
/// under the `cognition` feature. This is the in-Henosis counterpart to
/// [`HttpKleosClient`], supporting both standalone and local-store deployments
/// through the same trait.
pub struct InProcessKleosClient {
    /// In-process Chiasm task store.
    chiasm: Arc<ChiasmStore>,
    /// In-process Broca action/narration store.
    broca: Arc<BrocaStore>,
    /// The memory backend (HTTP to upstream Kleos, or in-process cognition).
    memory: Arc<dyn BridgeMemory>,
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
    /// Build from injected kernel store handles, a memory backend, a tenant, and
    /// the bridge service principal.
    pub fn new(
        chiasm: Arc<ChiasmStore>,
        broca: Arc<BrocaStore>,
        memory: Arc<dyn BridgeMemory>,
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
    /// Search memory through the configured backend (upstream Kleos over HTTP, or
    /// the in-process cognition store under the `cognition` feature).
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
        let rows = self.memory.search(&query, limit).await?;
        Ok(rows
            .into_iter()
            .map(|row| format!("[memory:{} source={}] {}", row.id, row.source, row.content))
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
            .list(self.tenant, self.principal, filter)
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

    /// Store a consensus memory through the configured backend. The backend keeps
    /// `category: decision` / `importance: 6`; this method only ensures the
    /// `project:<project>` tag is present.
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
        self.memory.store(content, &all_tags).await
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
            .update(self.tenant, self.principal, id, patch)
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
    format!("#{channel} {joined}")
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

    /// Build an in-process client over fresh in-memory stores. The HTTP memory
    /// backend points at an unused address; the tests below never call a memory
    /// method (the Chiasm/Broca paths are what is exercised here).
    fn client() -> (InProcessKleosClient, TenantId, PrincipalId) {
        let bus = Arc::new(AxonBus::new());
        let chiasm = Arc::new(ChiasmStore::open_in_memory(bus.clone()).unwrap());
        let broca = Arc::new(BrocaStore::open_in_memory(bus).unwrap());
        let memory: Arc<dyn BridgeMemory> = Arc::new(HttpMemoryBackend::new(Arc::new(
            MemoryClient::new("http://127.0.0.1:1".to_string(), None, None),
        )));
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
        let summary = kleos
            .active_tasks_summary("proj", 10)
            .await
            .expect("summary");
        assert!(
            summary.is_none(),
            "blocked_on_human task must not be active"
        );
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
        let summary = kleos
            .active_tasks_summary("proj", 10)
            .await
            .expect("summary");
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

    /// An Axon wire event flattens project and summary out of the activity
    /// payload; absent payload fields become None instead of erroring.
    #[test]
    fn axon_event_brief_flattens_payload() {
        let full: super::KleosAxonEvent = serde_json::from_value(serde_json::json!({
            "id": 42,
            "channel": "alerts",
            "action": "error.raised",
            "agent": "claude-code",
            "payload": {
                "agent": "claude-code",
                "action": "error.raised",
                "summary": "build broke",
                "project": "henosis"
            }
        }))
        .expect("wire event parses");
        let brief = super::AxonEventBrief::from(full);
        assert_eq!(brief.id, 42);
        assert_eq!(brief.channel, "alerts");
        assert_eq!(brief.project.as_deref(), Some("henosis"));
        assert_eq!(brief.summary.as_deref(), Some("build broke"));

        let bare: super::KleosAxonEvent = serde_json::from_value(serde_json::json!({
            "id": 43,
            "channel": "system",
            "action": "custom.thing"
        }))
        .expect("payload-less event parses");
        let brief = super::AxonEventBrief::from(bare);
        assert!(brief.project.is_none());
        assert!(brief.summary.is_none());
        assert!(brief.agent.is_none());
    }

    /// A Loom wire run narrows to the transition-relevant fields, and the
    /// response envelope tolerates an absent runs array.
    #[test]
    fn loom_run_brief_narrows_wire_row() {
        let run: super::KleosLoomRun = serde_json::from_value(serde_json::json!({
            "id": 7,
            "workflow_id": 3,
            "status": "failed",
            "error": "step 2 exploded",
            "input": {},
            "output": {},
            "user_id": 1,
            "created_at": "2026-07-19"
        }))
        .expect("wire run parses with extra fields");
        let brief = super::LoomRunBrief::from(run);
        assert_eq!(brief.id, 7);
        assert_eq!(brief.workflow_id, 3);
        assert_eq!(brief.status, "failed");
        assert_eq!(brief.error.as_deref(), Some("step 2 exploded"));

        let empty: super::KleosLoomRunsResponse =
            serde_json::from_value(serde_json::json!({})).expect("empty envelope parses");
        assert!(empty.runs.is_empty());
    }
}
