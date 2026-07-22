//! Task lifecycle: create, poll, pause, resume, and the async execution loop.
//! The `AppState` holds all mutable task state and the SSE stream hub.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::RwLock;
use tokio::sync::oneshot;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::checkpoint::{Checkpoint, PausedState};
use crate::clients::{AnthropicResult, ClientError, Clients};
use crate::hermes_client::{ToolDef, builtin_tools};
use crate::streaming::{StreamEventEnvelope, StreamHub};

/// Lifecycle state of a task. Transitions are monotone: Accepted ->
/// Running -> Completed/Failed, with a Paused detour on ask_human.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// Task accepted by the API; executor has not yet started.
    Accepted,
    /// Executor is actively running the tool-use loop.
    Running,
    /// Executor suspended on ask_human waiting for human input.
    Paused,
    /// Executor finished successfully.
    Completed,
    /// Executor encountered an unrecoverable error.
    Failed,
}

/// Persisted state for a single task. Serialized into Kleos for crash
/// recovery and returned verbatim from `GET /tasks/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// UUID assigned at task creation time.
    pub id: String,
    /// Current lifecycle state.
    pub status: TaskStatus,
    /// Optional tenant scope for multi-tenant credential resolution.
    pub tenant_id: Option<String>,
    /// Agent name forwarded to Chiasm when creating tasks.
    pub agent: String,
    /// Project name forwarded to Chiasm.
    pub project: String,
    /// Short human-readable task title.
    pub title: String,
    /// Original user prompt submitted to the executor.
    pub input: String,
    /// Optional extra system prompt appended to the identity block.
    pub system: Option<String>,
    /// Final assistant output, set on Completed.
    pub output: Option<String>,
    /// Human-readable error message, set on Failed.
    pub error: Option<String>,
    /// Chiasm task id for output submission, set once the task starts.
    pub chiasm_id: Option<i64>,
    /// agent-forge spec id, set at creation if agent-forge is enabled.
    #[serde(default)]
    pub spec_id: Option<String>,
    /// Optional shell command to run via agent-forge `verify` before the
    /// task transitions to Completed. If verify exits non-zero the task
    /// is marked Failed.
    #[serde(default)]
    pub verify_command: Option<String>,
    /// Wall-clock time the task was accepted.
    pub created_at: DateTime<Utc>,
    /// Wall-clock time the task was last mutated.
    pub updated_at: DateTime<Utc>,
}

/// Request body for `POST /tasks`.
#[derive(Debug, Deserialize)]
pub struct CreateTaskBody {
    /// Overrides the default Chiasm agent name for this task.
    pub agent: Option<String>,
    /// Overrides the default Chiasm project for this task.
    pub project: Option<String>,
    /// Short human-readable task title shown in Chiasm.
    pub title: Option<String>,
    /// Optional tenant scope for multi-tenant credential resolution.
    pub tenant_id: Option<String>,
    /// Optional extra system prompt appended to the identity block.
    pub system: Option<String>,
    /// User prompt that drives the agent loop.
    pub input: String,
    /// Optional shell command to run via agent-forge `verify` before the
    /// task transitions to Completed. If verify exits non-zero the task
    /// is marked Failed.
    pub verify_command: Option<String>,
}

/// Request body for `POST /tasks/{id}/resume`.
#[derive(Debug, Deserialize)]
pub struct ResumeBody {
    /// Human reply to the ask_human question. May be absent if the operator
    /// wants to unblock the task with no input.
    pub input: Option<String>,
}

/// In-memory task store. Thread-safe via `RwLock`; designed for single-node
/// use. Crash recovery is handled by Kleos checkpoints, not this store.
#[derive(Default)]
pub struct TaskStore {
    /// Active task records, keyed by task id.
    inner: RwLock<HashMap<String, TaskRecord>>,
    /// One-shot senders waiting for a human resume signal, keyed by task id.
    resume_senders: RwLock<HashMap<String, oneshot::Sender<Option<String>>>>,
}

/// Provides concurrent task record and resume-sender operations.
impl TaskStore {
    /// Insert or replace a task record.
    pub async fn insert(&self, rec: TaskRecord) {
        self.inner.write().await.insert(rec.id.clone(), rec);
    }

    /// Retrieve a task record by id, returning None if not found.
    pub async fn get(&self, id: &str) -> Option<TaskRecord> {
        self.inner.read().await.get(id).cloned()
    }

    /// Apply a mutating closure to a task record and update `updated_at`.
    pub async fn update<F: FnOnce(&mut TaskRecord)>(&self, id: &str, f: F) {
        if let Some(rec) = self.inner.write().await.get_mut(id) {
            f(rec);
            rec.updated_at = Utc::now();
        }
    }

    /// Register the one-shot sender that unblocks a paused task.
    pub async fn store_sender(&self, id: &str, sender: oneshot::Sender<Option<String>>) {
        self.resume_senders
            .write()
            .await
            .insert(id.to_string(), sender);
    }

    /// Remove and return the one-shot sender for a task. Returns None if no
    /// pending pause exists (task already resumed or not paused).
    pub async fn take_sender(&self, id: &str) -> Option<oneshot::Sender<Option<String>>> {
        self.resume_senders.write().await.remove(id)
    }
}

/// Shared application state threaded through all axum handlers.
#[derive(Clone)]
pub struct AppState {
    /// Aggregated client bundle (config, auth, Hermes, Kleos, Chiasm, Axon).
    pub clients: Arc<Clients>,
    /// In-memory task store.
    pub store: Arc<TaskStore>,
    /// Per-task SSE broadcast hub. Held by `AppState` so the streaming
    /// endpoint can subscribe and the task loop can emit terminal events.
    pub streams: Arc<StreamHub>,
}

/// Core task setup and execution, shared by [`create_task`] and [`run_task_to_completion`].
///
/// Creates the task record (including agent-forge spec registration), inserts it into the
/// store, mirrors it to Kleos, pre-registers the SSE sink, and then drives [`run_task`] to
/// completion. Returns when the task reaches a terminal state (Completed or Failed).
///
/// Callers are responsible for computing `agent`, `project`, and `title` from the body
/// and config before calling this function, and for performing any pre-flight validation
/// (empty-input check, provider-token check) so HTTP callers get immediate errors.
/// Synchronous task setup: registers the agent-forge spec, builds and stores the
/// [`TaskRecord`], mirrors it to Kleos, and pre-registers the SSE broadcast sink.
///
/// This is the part that MUST complete before the HTTP `POST /tasks` handler returns,
/// so a client subscribing to `GET /tasks/{id}/stream` immediately after the 202 finds
/// both the task record and its stream channel (otherwise it races the executor and 404s).
async fn setup_task(
    state: &AppState,
    id: &str,
    agent: String,
    project: &str,
    title: &str,
    body: &CreateTaskBody,
) {
    let cfg = state.clients.config();
    let now = Utc::now();

    // Register the task in agent-forge before any work begins. Best-effort:
    // a failure does not prevent the task from running, but the spec_id is
    // captured on the record so the audit trail links back.
    let spec_id = if cfg.agent_forge_enabled {
        state
            .clients
            .agent_forge()
            .spec_task(id, title, &body.input)
            .await
    } else {
        None
    };

    let rec = TaskRecord {
        id: id.to_string(),
        status: TaskStatus::Accepted,
        tenant_id: body.tenant_id.clone(),
        agent,
        project: project.to_string(),
        title: title.to_string(),
        input: body.input.clone(),
        system: body.system.clone(),
        output: None,
        error: None,
        chiasm_id: None,
        spec_id,
        verify_command: body.verify_command.clone(),
        created_at: now,
        updated_at: now,
    };
    state.store.insert(rec.clone()).await;
    state.clients.kleos_store_task(&rec).await;

    // Pre-register the SSE broadcast channel so a client that subscribes
    // to /tasks/{id}/stream immediately after creation sees the channel
    // even if the executor has not yet reached its first `streams.sink()` call.
    let _ = state.streams.sink(id).await;
}

/// Drives one prepared task through the orchestrator and records its terminal state.
async fn execute_task(
    state: AppState,
    id: String,
    agent: String,
    project: String,
    title: String,
    body: CreateTaskBody,
) {
    setup_task(&state, &id, agent, &project, &title, &body).await;
    run_task(
        state,
        id,
        project,
        title,
        body.tenant_id,
        body.system,
        body.input,
    )
    .await;
}

/// Submit a task and run it to a terminal state in-process, returning the final [`TaskRecord`].
///
/// This is the entry point for in-process callers -- notably the Loom Hephaestus step
/// executor in syntheos-server -- that need to dispatch an agent task and block until it
/// completes or fails. Unlike the HTTP `POST /tasks` handler, this function awaits the full
/// execution loop before returning.
///
/// The axum [`create_task`] handler shares the same [`execute_task`] core (via
/// `tokio::spawn`) so task setup logic is not duplicated.
///
/// Returns `Err(message)` only for pre-flight failures (empty input, no provider token);
/// a task that starts running and then fails returns `Ok(record)` with
/// `record.status == TaskStatus::Failed`.
pub async fn run_task_to_completion(
    state: AppState,
    body: CreateTaskBody,
) -> Result<TaskRecord, String> {
    if body.input.trim().is_empty() {
        return Err("input is required".to_string());
    }

    // Fail fast: verify the provider can yield a token before creating any record.
    state
        .clients
        .anthropic_token(body.tenant_id.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    let cfg = state.clients.config();
    let id = Uuid::new_v4().to_string();
    let agent = body
        .agent
        .clone()
        .unwrap_or_else(|| cfg.chiasm_agent.clone());
    let project = body
        .project
        .clone()
        .unwrap_or_else(|| cfg.chiasm_project.clone());
    let title = body
        .title
        .clone()
        .unwrap_or_else(|| format!("hephaestus task {}", &id[..8]));

    // Drive execute_task directly (no spawn) -- awaits the terminal state in-process.
    execute_task(state.clone(), id.clone(), agent, project, title, body).await;

    // Read and return the final record. The store must have it unless something
    // went catastrophically wrong inside execute_task.
    state
        .store
        .get(&id)
        .await
        .ok_or_else(|| "task record vanished after execution".to_string())
}

/// Axum handler for `POST /tasks`. Validates the body, fails fast if no provider token is
/// available, then spawns [`execute_task`] asynchronously and returns 202 Accepted immediately.
///
/// The 202 response carries the task id so the caller can poll `GET /tasks/{id}` or subscribe
/// to `GET /tasks/{id}/stream` for the terminal state.
pub async fn create_task(
    State(state): State<AppState>,
    Json(body): Json<CreateTaskBody>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if body.input.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "input is required" })),
        ));
    }

    // Fail fast at submission time so HTTP callers get an immediate error (503) rather
    // than a task that starts and immediately fails.
    if let Err(e) = state
        .clients
        .anthropic_token(body.tenant_id.as_deref())
        .await
    {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": e.to_string() })),
        ));
    }

    let cfg = state.clients.config();
    let id = Uuid::new_v4().to_string();
    let agent = body
        .agent
        .clone()
        .unwrap_or_else(|| cfg.chiasm_agent.clone());
    let project = body
        .project
        .clone()
        .unwrap_or_else(|| cfg.chiasm_project.clone());
    let title = body
        .title
        .clone()
        .unwrap_or_else(|| format!("hephaestus task {}", &id[..8]));

    // Setup synchronously (store insert + SSE sink registration) so a client that
    // subscribes to /tasks/{id}/stream immediately after the 202 finds the task and its
    // stream channel. Only the run loop is spawned, so the HTTP response stays immediate.
    setup_task(&state, &id, agent, &project, &title, &body).await;
    let state_spawn = state.clone();
    let id_spawn = id.clone();
    let project_spawn = project.clone();
    let title_spawn = title.clone();
    tokio::spawn(async move {
        run_task(
            state_spawn,
            id_spawn,
            project_spawn,
            title_spawn,
            body.tenant_id,
            body.system,
            body.input,
        )
        .await;
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "task_id": id, "status": "accepted" })),
    ))
}

/// Axum handler for `GET /tasks/{id}`. Returns 404 if the task id is not
/// in the in-memory store (task never existed or binary restarted).
pub async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<TaskRecord>, (StatusCode, Json<serde_json::Value>)> {
    state
        .store
        .get(&id)
        .await
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))))
}

/// Axum handler for `POST /tasks/{id}/resume`. Unblocks a paused task by
/// sending the human's reply through the stored one-shot channel. Returns
/// 404 for unknown ids, 409 if the task is not in Paused state.
pub async fn resume_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ResumeBody>>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let rec = state
        .store
        .get(&id)
        .await
        .ok_or((StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))))?;

    if rec.status != TaskStatus::Paused {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({ "error": "task is not paused", "status": rec.status })),
        ));
    }

    let sender = state.store.take_sender(&id).await.ok_or((
        StatusCode::CONFLICT,
        Json(json!({ "error": "no pending pause for this task" })),
    ))?;

    let human_input = body.and_then(|b| b.0.input);
    // If send fails the task has already moved on; ignore the error.
    let _ = sender.send(human_input);

    // Return the current record (status will flip to running asynchronously).
    let updated = state.store.get(&id).await.unwrap_or(rec);
    Ok((
        StatusCode::OK,
        Json(json!({ "task_id": id, "status": updated.status })),
    ))
}

/// Spawn a fresh executor loop for a newly-created task. Creates the Chiasm
/// task, flips status to Running, then calls into the LLM loop.
async fn run_task(
    state: AppState,
    id: String,
    project: String,
    title: String,
    tenant_id: Option<String>,
    system: Option<String>,
    input: String,
) {
    let clients = state.clients.clone();
    let store = state.store.clone();

    let summary = format!("hephaestus:{}", &id[..8]);
    let chiasm_id = clients.chiasm_create_task(&title, &summary, &project).await;
    store
        .update(&id, |r| {
            r.status = TaskStatus::Running;
            r.chiasm_id = chiasm_id;
        })
        .await;
    mirror_to_kleos(&clients, &store, &id).await;
    clients
        .axon_publish(
            "hephaestus.tasks",
            "task.started",
            json!({ "task_id": id, "title": title, "project": project }),
        )
        .await;

    let tools = builtin_tools();
    let max_turns = clients.config().max_tool_turns;
    let sink = state.streams.sink(&id).await;

    let llm_result = clients
        .anthropic_complete(
            tenant_id.as_deref(),
            Some(&id),
            &input,
            system.as_deref(),
            &tools,
            max_turns,
            Some(&sink),
        )
        .await;

    run_task_loop(
        state, id, project, chiasm_id, tenant_id, system, input, tools, max_turns, llm_result,
        false,
    )
    .await;
}

/// Public entry point used by main.rs on startup to resume a task whose state
/// was checkpointed to Kleos before a crash. Handles three cases:
/// - Accepted: re-spawn from the original input (effectively a fresh run).
/// - Running: load latest checkpoint, call anthropic_resume with its messages.
/// - Paused: load latest checkpoint, synthesize an AnthropicResult::Paused so
///   the loop body re-arms the resume oneshot.
pub async fn resume_task_from_kleos(state: AppState, rec: TaskRecord) {
    let clients = state.clients.clone();
    let id = rec.id.clone();

    if matches!(rec.status, TaskStatus::Completed | TaskStatus::Failed) {
        return;
    }

    clients
        .axon_publish(
            "hephaestus.tasks",
            "task.resumed_from_checkpoint",
            json!({
                "task_id": id,
                "previous_status": rec.status,
                "title": rec.title,
            }),
        )
        .await;

    // Accepted means the loop never started -- spawn fresh.
    if matches!(rec.status, TaskStatus::Accepted) {
        info!(task_id = %id, "resuming Accepted task -- starting fresh");
        run_task(
            state,
            id,
            rec.project,
            rec.title,
            rec.tenant_id,
            rec.system,
            rec.input,
        )
        .await;
        return;
    }

    let cp = match clients.kleos_load_latest_checkpoint(&id).await {
        Some(c) => c,
        None => {
            warn!(task_id = %id, "no checkpoint -- restarting from input");
            run_task(
                state,
                id,
                rec.project,
                rec.title,
                rec.tenant_id,
                rec.system,
                rec.input,
            )
            .await;
            return;
        }
    };

    let tools = builtin_tools();
    let max_turns = clients.config().max_tool_turns;
    let store = state.store.clone();

    if let Some(paused) = cp.paused.clone() {
        info!(task_id = %id, "resuming Paused task from checkpoint -- re-arming oneshot");
        let synthetic = Ok(AnthropicResult::Paused {
            accumulated_text: cp.accumulated_text,
            messages: cp.messages,
            question: paused.question,
            tool_use_id: paused.tool_use_id,
        });
        // skip_initial_chiasm=true so we don't create a duplicate HITL alert.
        run_task_loop(
            state,
            id,
            rec.project,
            rec.chiasm_id,
            cp.tenant_id,
            cp.system,
            rec.input,
            tools,
            max_turns,
            synthetic,
            true,
        )
        .await;
    } else {
        info!(task_id = %id, "resuming Running task from checkpoint");
        store.update(&id, |r| r.status = TaskStatus::Running).await;
        let sink = state.streams.sink(&id).await;
        let llm_result = clients
            .anthropic_resume(
                cp.tenant_id.as_deref(),
                Some(&id),
                cp.system.as_deref(),
                cp.messages,
                &tools,
                max_turns,
                cp.step.saturating_add(1),
                Some(&sink),
            )
            .await;
        run_task_loop(
            state,
            id,
            rec.project,
            rec.chiasm_id,
            cp.tenant_id,
            cp.system,
            rec.input,
            tools,
            max_turns,
            llm_result,
            false,
        )
        .await;
    }
}

/// Drive the task state machine from an initial LLM result through all
/// subsequent iterations (tool calls, HITL pauses, completion). Loops until
/// the task reaches a terminal state (Completed or Failed).
#[allow(clippy::too_many_arguments)]
async fn run_task_loop(
    state: AppState,
    id: String,
    project: String,
    chiasm_id: Option<i64>,
    tenant_id: Option<String>,
    system: Option<String>,
    input: String,
    tools: Vec<ToolDef>,
    max_turns: usize,
    mut llm_result: Result<AnthropicResult, ClientError>,
    mut skip_initial_chiasm: bool,
) {
    let clients = state.clients.clone();
    let store = state.store.clone();
    let streams = state.streams.clone();
    let sink = streams.sink(&id).await;

    loop {
        match llm_result {
            Ok(AnthropicResult::Complete(output)) => {
                info!(task_id = %id, len = output.len(), "task llm ok");

                // Run the optional agent-forge `verify` gate before marking
                // Complete. If verify fails the task is Failed, not Completed.
                let verify_command = store.get(&id).await.and_then(|r| r.verify_command.clone());
                let verify_passed = match verify_command.as_deref() {
                    Some(cmd) if clients.config().agent_forge_enabled => {
                        let ok = clients.agent_forge().verify(cmd).await;
                        if !ok {
                            warn!(task_id = %id, command = cmd, "verify failed");
                        }
                        ok
                    }
                    _ => true,
                };

                if !verify_passed {
                    let msg = format!(
                        "verify command failed: {}",
                        verify_command.unwrap_or_default()
                    );
                    sink.emit(StreamEventEnvelope::TaskFailed { error: msg.clone() });
                    clients
                        .axon_publish(
                            "hephaestus.tasks",
                            "task.failed",
                            json!({ "task_id": id, "error": msg, "phase": "verify" }),
                        )
                        .await;
                    if let Some(cid) = chiasm_id {
                        clients
                            .chiasm_submit_output(cid, &format!("FAILED (verify): {msg}"))
                            .await;
                    }
                    store
                        .update(&id, |r| {
                            r.status = TaskStatus::Failed;
                            r.error = Some(msg);
                            r.output = Some(output.clone());
                        })
                        .await;
                    mirror_to_kleos(&clients, &store, &id).await;
                    streams.close(&id).await;
                    break;
                }

                // Mark Completed and mirror to Kleos FIRST. A crash between
                // here and the thread/chiasm writes below leaves the task in
                // Completed state, which the recovery scan correctly skips.
                store
                    .update(&id, |r| {
                        r.status = TaskStatus::Completed;
                        r.output = Some(output.clone());
                    })
                    .await;
                mirror_to_kleos(&clients, &store, &id).await;
                let thread_content = format!("USER:\n{input}\n\nASSISTANT:\n{output}");
                clients.kleos_store_thread(&id, &thread_content).await;
                if let Some(cid) = chiasm_id {
                    clients.chiasm_submit_output(cid, &output).await;
                }
                clients
                    .axon_publish(
                        "hephaestus.tasks",
                        "task.completed",
                        json!({ "task_id": id, "bytes": output.len() }),
                    )
                    .await;
                sink.emit(StreamEventEnvelope::TaskComplete {
                    output: output.clone(),
                });
                streams.close(&id).await;
                break;
            }

            Ok(AnthropicResult::Paused {
                accumulated_text,
                messages,
                question,
                tool_use_id,
            }) => {
                info!(task_id = %id, %question, "task paused -- waiting for human");
                sink.emit(StreamEventEnvelope::TaskPaused {
                    question: question.clone(),
                });

                // Store partial output so GET /tasks/{id} shows progress.
                if !accumulated_text.is_empty() {
                    store
                        .update(&id, |r| r.output = Some(accumulated_text.clone()))
                        .await;
                }

                // Stage the resume oneshot BEFORE flipping status to Paused.
                // Otherwise a client that observes Paused via GET /tasks/:id
                // and immediately POSTs /resume could arrive before
                // store_sender has run, returning 409.
                let (tx, rx) = oneshot::channel::<Option<String>>();
                store.store_sender(&id, tx).await;

                // Set status to Paused and mirror.
                store.update(&id, |r| r.status = TaskStatus::Paused).await;
                mirror_to_kleos(&clients, &store, &id).await;

                // Persist a paused checkpoint so a crash here can re-arm the
                // oneshot on restart.
                let paused_cp = Checkpoint {
                    task_id: id.clone(),
                    step: 0,
                    messages: messages.clone(),
                    accumulated_text: accumulated_text.clone(),
                    tenant_id: tenant_id.clone(),
                    system: system.clone(),
                    paused: Some(PausedState {
                        question: question.clone(),
                        tool_use_id: tool_use_id.clone(),
                    }),
                    created_at: chrono::Utc::now(),
                };
                clients.kleos_store_checkpoint(&paused_cp).await;

                // Create a Chiasm task to alert the operator (skipped on
                // crash-resume so we don't double-alert).
                if !skip_initial_chiasm {
                    let hitl_title = format!("HITL: {}", &question[..question.len().min(80)]);
                    let hitl_summary = format!("task_id={id}\n\n{question}");
                    clients
                        .chiasm_create_task(&hitl_title, &hitl_summary, &project)
                        .await;
                }

                // Block until resume_task sends the human response.
                let human_input = rx.await.unwrap_or(None);

                // Resume: set status back to Running.
                store.update(&id, |r| r.status = TaskStatus::Running).await;
                mirror_to_kleos(&clients, &store, &id).await;

                // Append the human's response as the tool_result for ask_human.
                let human_content =
                    human_input.unwrap_or_else(|| "(no response provided)".to_string());
                let mut resumed_messages = messages;
                resumed_messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": human_content,
                        "is_error": false
                    }]
                }));

                info!(task_id = %id, "task resuming after human input");
                llm_result = clients
                    .anthropic_resume(
                        tenant_id.as_deref(),
                        Some(&id),
                        system.as_deref(),
                        resumed_messages,
                        &tools,
                        max_turns,
                        0,
                        Some(&sink),
                    )
                    .await;
                // After the first synthetic-paused iteration the rest of the
                // run is "normal" -- chiasm alerts on subsequent pauses are OK.
                skip_initial_chiasm = false;
            }

            Err(e) => {
                let msg = match &e {
                    ClientError::Anthropic { status, body } => {
                        format!("anthropic {status}: {body}")
                    }
                    other => other.to_string(),
                };
                error!(task_id = %id, error = %msg, "task failed");
                sink.emit(StreamEventEnvelope::TaskFailed { error: msg.clone() });
                clients
                    .axon_publish(
                        "hephaestus.tasks",
                        "task.failed",
                        json!({ "task_id": id, "error": msg }),
                    )
                    .await;
                if let Some(cid) = chiasm_id {
                    clients
                        .chiasm_submit_output(cid, &format!("FAILED: {msg}"))
                        .await;
                }
                store
                    .update(&id, |r| {
                        r.status = TaskStatus::Failed;
                        r.error = Some(msg);
                    })
                    .await;
                mirror_to_kleos(&clients, &store, &id).await;
                streams.close(&id).await;
                warn!(task_id = %id, "task marked failed");
                break;
            }
        }
    }
}

/// Convenience: fetch the current record and mirror it to Kleos.
async fn mirror_to_kleos(clients: &Clients, store: &TaskStore, id: &str) {
    if let Some(rec) = store.get(id).await {
        clients.kleos_store_task(&rec).await;
    }
}
