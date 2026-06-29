//! Per-task SSE streaming. Each running task gets a `broadcast::Sender`
//! keyed by task id; SSE subscribers receive events as the orchestrator
//! emits them. Subscribers that lag or disconnect are silently dropped --
//! task execution must never depend on a listener being attached.
//!
//! Phase 1 emits synthesized events derived from the orchestrator's
//! non-streaming `Provider::send` results: text_delta per text block,
//! tool_start per tool_use, plus turn_end / task_complete / task_paused /
//! task_failed. When `Provider::send_streaming` is wired into the
//! orchestrator (later phase) the synthetic events will be replaced with
//! real per-token deltas with no schema change for clients.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::{RwLock, broadcast};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tracing::warn;

/// Single SSE event emitted to subscribers. The wire shape is the JSON
/// serialization of this enum, with the variant tag in `type` and the
/// remaining fields adjacent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEventEnvelope {
    /// Partial assistant text. Multiple events per turn possible.
    TextDelta { text: String },
    /// Model began invoking a tool.
    ToolStart { id: String, name: String },
    /// Tool result returned (after dispatch). `is_error` mirrors the tool
    /// dispatcher's flag.
    ToolResult {
        /// Matches the tool_use_id from the initiating ToolStart.
        id: String,
        /// Serialized tool result or error payload.
        content: String,
        /// True when the tool invocation itself failed.
        is_error: bool,
    },
    /// LLM turn finished. `stop_reason` is the lowercased Provider
    /// `StopReason` (end_turn / tool_use / max_tokens / stop_sequence).
    TurnEnd { stop_reason: String },
    /// Task completed successfully. Final assistant output.
    TaskComplete { output: String },
    /// Task hit ask_human and is waiting for input.
    TaskPaused { question: String },
    /// Task failed. `error` is the human-readable message.
    TaskFailed { error: String },
}

/// Buffer size per task channel. Subscribers that lag beyond this window
/// receive a `Lagged` error and the broadcast layer drops oldest events.
/// 256 events is enough for a turn of moderately verbose output without
/// stalling the orchestrator if a subscriber drains slowly.
const CHANNEL_CAPACITY: usize = 256;

/// Hub that tracks one broadcast channel per active task. Senders are
/// removed when a task closes its sink so old task ids don't accumulate.
#[derive(Default)]
pub struct StreamHub {
    /// Map from task id to the live broadcast sender for that task.
    channels: RwLock<HashMap<String, broadcast::Sender<StreamEventEnvelope>>>,
}

/// Stream-hub operations. Adding, subscribing, and closing channels are
/// the entire surface; the underlying broadcast::Sender is never exposed.
impl StreamHub {
    /// Construct an empty hub. Held inside `AppState` as `Arc<StreamHub>`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Open or reuse the broadcast channel for a task. Returns a
    /// `StreamSink` that the orchestrator calls into. Multiple sinks for
    /// the same task share the same broadcast::Sender.
    pub async fn sink(self: &Arc<Self>, task_id: &str) -> StreamSink {
        let mut guard = self.channels.write().await;
        let sender = guard
            .entry(task_id.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .clone();
        StreamSink {
            task_id: task_id.to_string(),
            sender,
        }
    }

    /// Subscribe to a task's events. Returns None if no channel exists for
    /// that id (task already closed or never started).
    pub async fn subscribe(
        &self,
        task_id: &str,
    ) -> Option<broadcast::Receiver<StreamEventEnvelope>> {
        let guard = self.channels.read().await;
        guard.get(task_id).map(|s| s.subscribe())
    }

    /// Remove a task's channel. Called once the task reaches a terminal
    /// state (Completed / Failed). Late subscribers will get None from
    /// `subscribe`; that's intentional -- they should look at GET /tasks/:id
    /// for the final record instead.
    pub async fn close(&self, task_id: &str) {
        let mut guard = self.channels.write().await;
        guard.remove(task_id);
    }
}

/// Sink the orchestrator emits into. A no-op `send` when no receivers are
/// attached -- we never fail a task because nobody's listening.
#[derive(Clone)]
pub struct StreamSink {
    /// Task id this sink is bound to. Diagnostic only.
    task_id: String,
    /// Underlying broadcast sender. Cheap to clone (just an Arc handle).
    sender: broadcast::Sender<StreamEventEnvelope>,
}

/// Per-task emit interface used by the orchestrator and the task loop.
/// Cheap to clone (just a `broadcast::Sender` handle plus the task id).
impl StreamSink {
    /// Emit one event. Drops are silent: broadcast::Sender::send returns
    /// Err only when there are no live receivers, which is fine.
    pub fn emit(&self, ev: StreamEventEnvelope) {
        let _ = self.sender.send(ev);
    }

    /// Borrow the task id this sink is bound to. Diagnostic only.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
}

/// Axum handler for `GET /tasks/{id}/stream`. Returns 404 if the task has
/// no live channel (either unknown id or already terminated). On success,
/// returns an SSE stream that emits one event per `StreamEventEnvelope`
/// until the broadcast::Sender is dropped or all senders close.
pub async fn stream_task(
    State(state): State<crate::tasks::AppState>,
    Path(id): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    let rx = state
        .streams
        .subscribe(&id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    // Map broadcast events to SSE events. `BroadcastStream` surfaces
    // lag errors as `Err(BroadcastStreamRecvError::Lagged(n))`; those are
    // logged and skipped so a slow subscriber never breaks the stream.
    let stream = BroadcastStream::new(rx).filter_map(|item| match item {
        Ok(envelope) => match Event::default().json_data(&envelope) {
            Ok(ev) => Some(Ok(ev)),
            Err(e) => {
                warn!(error = %e, "sse serialize event failed");
                None
            }
        },
        Err(e) => {
            warn!(error = %e, "sse subscriber lagged");
            None
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
