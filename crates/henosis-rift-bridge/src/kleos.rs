//! Kleos integration for bridge discussion context and consensus writeback.

use async_trait::async_trait;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

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
