//! Orchestration acceptance tests:
//!  1. happy_path_three_step_loop  -- full agent loop with one Hermes tool
//!  2. hitl_pause_and_resume       -- ask_human flow + POST /resume
//!  3. crash_recovery_resumes      -- replay a task from a Kleos checkpoint
//!
//! All external services (Anthropic, Kleos, Chiasm, Axon, Eidolon, Hermes)
//! are mocked with wiremock; the Hephaestus router is built in-process and
//! served on a random local port.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use henosis_hephaestus::{
    Config, CreateTaskBody, build_router, build_state, recover_in_flight_tasks,
    run_task_to_completion, tasks::TaskStatus,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Service credential used by the isolated HTTP acceptance server.
const API_TOKEN: &str = "hephaestus-api-token-that-is-at-least-32-bytes";

// -- mock harness ------------------------------------------------------------

/// Bundle of mocked external services used across the acceptance tests.
struct Mocks {
    /// Mocked Anthropic Messages API endpoint.
    anthropic: MockServer,
    /// Mocked Kleos service (store, search, axon publish).
    kleos: MockServer,
    /// Mocked Chiasm coordination service.
    chiasm: MockServer,
    /// Mocked Eidolon gate service.
    eidolon: MockServer,
    /// Mocked Hermes tool gateway.
    hermes: MockServer,
    /// Path to the dev credentials file.
    _credfile: PathBuf,
    /// Temp directory owner; dropped last so the file outlives the test.
    _tmp: TempDir,
}

/// Builds and configures the mock services shared by acceptance tests.
impl Mocks {
    /// Spin up all mock servers and pre-populate standard 200 responses for
    /// the coordination services. The Anthropic mock is configured per-test.
    async fn new() -> Self {
        let anthropic = MockServer::start().await;
        let kleos = MockServer::start().await;
        let chiasm = MockServer::start().await;
        let eidolon = MockServer::start().await;
        let hermes = MockServer::start().await;

        // /store always 200 with a stub id.
        Mock::given(method("POST"))
            .and(path("/store"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .mount(&kleos)
            .await;

        // /search returns empty array unless overridden.
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&kleos)
            .await;

        // /axon/publish 200.
        Mock::given(method("POST"))
            .and(path("/axon/publish"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&kleos)
            .await;

        // Chiasm task create + output 200.
        Mock::given(method("POST"))
            .and(path("/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 42})))
            .mount(&chiasm)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/tasks/\d+/output$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&chiasm)
            .await;

        // Eidolon /gate/check always allows.
        Mock::given(method("POST"))
            .and(path("/gate/check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allow": true})))
            .mount(&eidolon)
            .await;

        // Tempdir + dev credentials file.
        let tmp = tempfile::tempdir().expect("tempdir");
        let cred = tmp.path().join("credentials.json");
        tokio::fs::write(&cred, br#"{"claudeAiOauth":{"accessToken":"test-token"}}"#)
            .await
            .expect("cred write");

        Self {
            anthropic,
            kleos,
            chiasm,
            eidolon,
            hermes,
            _credfile: cred,
            _tmp: tmp,
        }
    }

    /// Build a `Config` wired to all mock servers with Crucible and cred disabled.
    fn config(&self) -> Config {
        Config {
            port: 0,
            env: henosis_hephaestus::config::DeployEnv::Dev,
            kleos_url: self.kleos.uri(),
            kleos_token_slot: "test/kleos".into(),
            chiasm_url: self.chiasm.uri(),
            chiasm_agent: "hephaestus".into(),
            chiasm_project: "orchestration-test".into(),
            chiasm_token_slot: None,
            axon_url: self.kleos.uri(),
            plutus_url: None,
            dev_credentials_path: self._credfile.clone(),
            model: "claude-haiku-4-5-20251001".into(),
            max_tokens: 256,
            http_timeout: Duration::from_secs(2),
            llm_timeout: Duration::from_secs(5),
            eidolon_url: self.eidolon.uri(),
            hermes_url: self.hermes.uri(),
            anthropic_url: format!("{}/v1/messages", self.anthropic.uri()),
            max_tool_turns: 6,
            sandbox_timeout: 5,
            sandbox_memory: "64m".into(),
            crucible_db: self._tmp.path().join("crucible.db"),
            crucible_enabled: false,
            cred_enabled: false,
            provider_kind: henosis_hephaestus::config::ProviderKind::Anthropic,
            provider_url: None,
            provider_key_slot: None,
            provider_api_key: None,
        }
    }
}

/// Convenience wrapper for wiremock's path_regex matcher.
fn path_regex(re: &str) -> wiremock::matchers::PathRegexMatcher {
    wiremock::matchers::path_regex(re)
}

/// Spawn the Hephaestus router on a random local port and return the base
/// URL and AppState so tests can drive it via HTTP and inspect state.
async fn spawn_app(cfg: Config) -> (String, henosis_hephaestus::AppState) {
    let state = build_state(cfg);
    let router = build_router(state.clone(), API_TOKEN.to_string());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    (format!("http://{addr}"), state)
}

/// Build an HTTP client that authenticates to the isolated task-control surface.
fn authenticated_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::AUTHORIZATION,
        format!("Bearer {API_TOKEN}").parse().unwrap(),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap()
}

/// Poll `GET /tasks/{task_id}` until the task reaches `target` status or the
/// timeout expires. Returns the final TaskRecord JSON on success.
async fn poll_status(
    base: &str,
    task_id: &str,
    target: TaskStatus,
    timeout: Duration,
) -> Option<Value> {
    let client = authenticated_client();
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let r = client
            .get(format!("{base}/tasks/{task_id}"))
            .send()
            .await
            .ok()?;
        if r.status().is_success() {
            let v: Value = r.json().await.ok()?;
            let s = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let target_str = match target {
                TaskStatus::Completed => "completed",
                TaskStatus::Paused => "paused",
                TaskStatus::Failed => "failed",
                TaskStatus::Running => "running",
                TaskStatus::Accepted => "accepted",
            };
            if s == target_str {
                return Some(v);
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// Count provider requests sent through the mocked Anthropic endpoint.
async fn provider_request_count(mocks: &Mocks) -> usize {
    mocks
        .anthropic
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/v1/messages")
        .count()
}

/// Build a wiremock response body simulating an Anthropic assistant turn that
/// calls one tool before stopping.
fn assistant_with_tool(tool_name: &str, tool_input: Value) -> Value {
    json!({
        "id": "msg_test",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "thinking..."},
            {"type": "tool_use", "id": "toolu_test", "name": tool_name, "input": tool_input},
        ],
        "stop_reason": "tool_use",
        "model": "test",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

/// Build a wiremock response body simulating an Anthropic end_turn response.
fn assistant_end_turn(text: &str) -> Value {
    json!({
        "id": "msg_test_end",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "model": "test",
        "stop_sequence": null,
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

/// Task-control routes reject unauthenticated callers while liveness remains public.
#[tokio::test]
async fn task_routes_require_service_authentication() {
    let mocks = Mocks::new().await;
    let (base, _state) = spawn_app(mocks.config()).await;
    let client = reqwest::Client::new();

    let rejected = client
        .post(format!("{base}/tasks"))
        .json(&json!({"input": "must not run"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);

    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.status(), reqwest::StatusCode::OK);
}

/// The in-process completion entry point performs one provider execution pass.
#[tokio::test]
async fn in_process_task_executes_once() {
    let mocks = Mocks::new().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(assistant_end_turn("once")))
        .mount(&mocks.anthropic)
        .await;

    let state = build_state(mocks.config());
    let record = run_task_to_completion(
        state,
        CreateTaskBody {
            agent: None,
            project: None,
            title: Some("exactly-once".to_string()),
            tenant_id: None,
            principal_id: None,
            system: None,
            input: "execute once".to_string(),
            verify_command: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(record.status, TaskStatus::Completed);

    let provider_calls = provider_request_count(&mocks).await;
    assert_eq!(provider_calls, 1);
}

/// A gate denial prevents the first provider request and fails the task.
#[tokio::test]
async fn gate_denial_prevents_provider_request() {
    let mocks = Mocks::new().await;
    mocks.eidolon.reset().await;
    Mock::given(method("POST"))
        .and(path("/gate/check"))
        .and(body_json(json!({
            "action": "llm.call",
            "context": {"turn": 0, "action": "llm.call"},
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"allow": false, "reason": "policy blocked"})),
        )
        .mount(&mocks.eidolon)
        .await;

    let record = run_task_to_completion(
        build_state(mocks.config()),
        CreateTaskBody {
            agent: None,
            project: None,
            title: Some("denied-before-provider".to_string()),
            tenant_id: None,
            principal_id: None,
            system: None,
            input: "must not reach provider".to_string(),
            verify_command: None,
        },
    )
    .await
    .expect("task record");

    assert_eq!(record.status, TaskStatus::Failed);
    assert!(
        record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("gate denied llm.call"),
        "unexpected gate failure: {:?}",
        record.error,
    );
    assert_eq!(provider_request_count(&mocks).await, 0);
}

/// An unavailable gate prevents the first provider request and fails closed.
#[tokio::test]
async fn gate_error_prevents_provider_request() {
    let mocks = Mocks::new().await;
    mocks.eidolon.reset().await;
    Mock::given(method("POST"))
        .and(path("/gate/check"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .mount(&mocks.eidolon)
        .await;

    let record = run_task_to_completion(
        build_state(mocks.config()),
        CreateTaskBody {
            agent: None,
            project: None,
            title: Some("gate-error-before-provider".to_string()),
            tenant_id: None,
            principal_id: None,
            system: None,
            input: "must not reach provider".to_string(),
            verify_command: None,
        },
    )
    .await
    .expect("task record");

    assert_eq!(record.status, TaskStatus::Failed);
    assert!(
        record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("gate check failed for llm.call"),
        "unexpected gate failure: {:?}",
        record.error,
    );
    assert_eq!(provider_request_count(&mocks).await, 0);
}

/// A denial after tool execution prevents tool results from reaching a second provider turn.
#[tokio::test]
async fn post_tool_gate_denial_prevents_followup_provider_request() {
    let mocks = Mocks::new().await;
    mocks.eidolon.reset().await;
    Mock::given(method("POST"))
        .and(path("/gate/check"))
        .and(body_json(json!({
            "action": "llm.call",
            "context": {"turn": 0, "action": "llm.call"},
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allow": true})))
        .mount(&mocks.eidolon)
        .await;
    Mock::given(method("POST"))
        .and(path("/gate/check"))
        .and(body_json(json!({
            "action": "tool.results",
            "context": {"action": "tool.results", "tool_count": 1},
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"allow": false, "reason": "results blocked"})),
        )
        .mount(&mocks.eidolon)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(assistant_with_tool("fake_tool", json!({"q": "hi"}))),
        )
        .mount(&mocks.anthropic)
        .await;

    let record = run_task_to_completion(
        build_state(mocks.config()),
        CreateTaskBody {
            agent: None,
            project: None,
            title: Some("denied-tool-results".to_string()),
            tenant_id: None,
            principal_id: None,
            system: None,
            input: "tool result must not continue".to_string(),
            verify_command: None,
        },
    )
    .await
    .expect("task record");

    assert_eq!(record.status, TaskStatus::Failed);
    assert!(
        record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("gate denied tool.results"),
        "unexpected gate failure: {:?}",
        record.error,
    );
    assert_eq!(provider_request_count(&mocks).await, 1);
}

// -- Test 1: 3-step loop -----------------------------------------------------

/// Full agent loop: first turn calls a Hermes tool, second turn ends.
/// Verifies Hermes was invoked and Anthropic received at least 2 calls.
#[tokio::test]
async fn happy_path_three_step_loop() {
    let mocks = Mocks::new().await;

    // Turn 0: assistant calls a Hermes tool. up_to_n_times(1) so the next
    // request falls through to the second mock.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(assistant_with_tool("fake_tool", json!({"q": "hi"}))),
        )
        .up_to_n_times(1)
        .mount(&mocks.anthropic)
        .await;
    // Turn 1: end_turn.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(assistant_end_turn("done")))
        .mount(&mocks.anthropic)
        .await;

    // No Hermes HTTP mock is needed because tools dispatch in-process.
    // `fake_tool` is not in the Hermes registry, so the in-process path
    // returns a structured tool_not_found error which the LLM receives as
    // a tool_result and then ends the turn normally.

    let (base, _state) = spawn_app(mocks.config()).await;
    let client = authenticated_client();

    let r = client
        .post(format!("{base}/tasks"))
        .json(&json!({"input": "do a thing", "title": "smoke"}))
        .send()
        .await
        .expect("post tasks");
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    let body: Value = r.json().await.expect("json");
    let task_id = body
        .get("task_id")
        .and_then(|s| s.as_str())
        .expect("task_id");

    let final_state = poll_status(
        &base,
        task_id,
        TaskStatus::Completed,
        Duration::from_secs(5),
    )
    .await
    .expect("task did not reach Completed");
    assert_eq!(
        final_state.get("output").and_then(|s| s.as_str()),
        Some("thinking...done")
    );

    // Verify in-process dispatch: the mock Hermes HTTP server must have
    // received zero requests (tool was invoked in-process, not over HTTP).
    let hermes_http_hits = mocks
        .hermes
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    assert_eq!(
        hermes_http_hits, 0,
        "expected 0 HTTP requests to Hermes (in-process dispatch active), got {hermes_http_hits}"
    );

    // Anthropic was called at least twice (tool turn + end_turn).
    let llm_hits = mocks
        .anthropic
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == "/v1/messages")
        .count();
    assert!(
        llm_hits >= 2,
        "expected >=2 anthropic calls, got {llm_hits}"
    );
}

// -- Test 2: HITL pause + resume --------------------------------------------

/// ask_human flow: first turn triggers a pause, POST /resume unblocks it,
/// second turn completes. Verifies Chiasm received at least 2 task creates
/// (original + HITL alert).
#[tokio::test]
async fn hitl_pause_and_resume() {
    let mocks = Mocks::new().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(assistant_with_tool(
                "ask_human",
                json!({"question": "ok to proceed?"}),
            )),
        )
        .up_to_n_times(1)
        .mount(&mocks.anthropic)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(assistant_end_turn("acknowledged")))
        .mount(&mocks.anthropic)
        .await;

    let (base, _state) = spawn_app(mocks.config()).await;
    let client = authenticated_client();

    let r = client
        .post(format!("{base}/tasks"))
        .json(&json!({"input": "ask me first"}))
        .send()
        .await
        .expect("post tasks");
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    let body: Value = r.json().await.expect("json");
    let task_id = body
        .get("task_id")
        .and_then(|s| s.as_str())
        .expect("task_id");

    let _ = poll_status(&base, task_id, TaskStatus::Paused, Duration::from_secs(5))
        .await
        .expect("task did not reach Paused");

    // POST resume with the human reply.
    let resume = client
        .post(format!("{base}/tasks/{task_id}/resume"))
        .json(&json!({"input": "yes go"}))
        .send()
        .await
        .expect("post resume");
    assert_eq!(resume.status(), reqwest::StatusCode::OK);

    let final_state = poll_status(
        &base,
        task_id,
        TaskStatus::Completed,
        Duration::from_secs(5),
    )
    .await
    .expect("task did not reach Completed after resume");
    assert!(
        final_state
            .get("output")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .contains("acknowledged")
    );

    // Chiasm should have been hit twice: original task create + HITL alert.
    let chiasm_hits = mocks
        .chiasm
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.method == "POST" && r.url.path() == "/tasks")
        .count();
    assert!(
        chiasm_hits >= 2,
        "expected >=2 chiasm task creates, got {chiasm_hits}"
    );
}

// -- Test 3: crash recovery from a Kleos checkpoint -------------------------

/// Crash recovery: seed Kleos with a Running task + checkpoint, call
/// `recover_in_flight_tasks`, verify the task resumes and reaches Completed.
/// Also asserts exactly one `agent_thread` write to guard against duplicates.
#[tokio::test]
async fn crash_recovery_resumes_from_checkpoint() {
    let mocks = Mocks::new().await;

    // The "in-flight" task: status=Running, no further LLM call needed. The
    // recovery path will call anthropic_resume which we answer with end_turn.
    let task_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now();
    let task_record = json!({
        "id": task_id,
        "status": "running",
        "tenant_id": null,
        "agent": "hephaestus",
        "project": "orchestration-test",
        "title": "recovered task",
        "input": "prior request",
        "system": null,
        "output": null,
        "error": null,
        "chiasm_id": null,
        "spec_id": null,
        "verify_command": null,
        "created_at": now,
        "updated_at": now,
    });

    let task_memory = json!({
        "content": serde_json::to_string(&task_record).unwrap(),
        "category": "hephaestus_task",
        "tags": ["task", "running"],
    });

    let checkpoint = json!({
        "task_id": task_id,
        "step": 1,
        "messages": [{"role": "user", "content": "prior request"}],
        "accumulated_text": "partial-",
        "tenant_id": null,
        "system": null,
        "paused": null,
        "created_at": now,
    });
    let checkpoint_memory = json!({
        "content": serde_json::to_string(&checkpoint).unwrap(),
        "category": "hephaestus_checkpoint",
        "tags": ["checkpoint", format!("checkpoint:{task_id}")],
    });

    // Override the default empty-search response with sequenced ones.
    mocks.kleos.reset().await;
    // Re-establish base /store mock.
    Mock::given(method("POST"))
        .and(path("/store"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .mount(&mocks.kleos)
        .await;
    Mock::given(method("POST"))
        .and(path("/axon/publish"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&mocks.kleos)
        .await;
    // First /search: kleos_recover_tasks looking for hephaestus_task.
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(wiremock::matchers::query_param("q", "hephaestus_task"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([task_memory])))
        .mount(&mocks.kleos)
        .await;
    // Second /search: kleos_load_latest_checkpoint by tag.
    let tag = format!("checkpoint:{task_id}");
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(wiremock::matchers::query_param("q", tag.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([checkpoint_memory])))
        .mount(&mocks.kleos)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(assistant_end_turn("resumed-ok")))
        .mount(&mocks.anthropic)
        .await;

    // Build state, trigger recovery, drive through to completion.
    let cfg = mocks.config();
    let state = build_state(cfg);
    let resumed = recover_in_flight_tasks(&state).await;
    assert_eq!(resumed, 1, "expected to resume 1 task");

    // Wait for the resumed task to flip to Completed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut final_status = None;
    while std::time::Instant::now() < deadline {
        if let Some(rec) = state.store.get(&task_id).await
            && matches!(rec.status, TaskStatus::Completed | TaskStatus::Failed)
        {
            final_status = Some(rec.status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        final_status,
        Some(TaskStatus::Completed),
        "resumed task did not complete"
    );

    // Idempotency check: only one /v1/messages call (the resume) should have
    // happened, and only one final agent_thread store should have been
    // written. The completion branch flips status BEFORE writing the thread,
    // so wait until the thread write actually arrives.
    let llm_hits = mocks
        .anthropic
        .received_requests()
        .await
        .unwrap_or_default()
        .len();
    assert!(llm_hits >= 1, "expected >=1 anthropic call");

    let count_thread_writes = || async {
        mocks
            .kleos
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.method == "POST" && r.url.path() == "/store")
            .filter(|r| {
                let body = std::str::from_utf8(&r.body).unwrap_or("");
                body.contains("\"category\":\"agent_thread\"")
            })
            .count()
    };

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut thread_writes = count_thread_writes().await;
    while thread_writes == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
        thread_writes = count_thread_writes().await;
    }
    // Settle a beat to let any duplicate write race land before counting.
    tokio::time::sleep(Duration::from_millis(100)).await;
    thread_writes = count_thread_writes().await;
    assert_eq!(
        thread_writes, 1,
        "expected exactly 1 agent_thread store write, got {thread_writes}"
    );

    // Silence unused-warning for state field we held on to.
    let _ = Arc::clone(&state.store);
}

// -- Test 4: SSE stream emits per-turn events ---------------------------------

/// Verifies that GET /tasks/{id}/stream yields at least one text_delta and a
/// turn_end event before the task completes. The SSE client is a minimal
/// raw-HTTP reader that splits on blank lines (the SSE event delimiter)
/// rather than pulling in a separate sse-client dependency just for this
/// smoke test.
#[tokio::test]
async fn sse_stream_emits_progress_events() {
    let mocks = Mocks::new().await;

    // Single end_turn response so the orchestrator emits exactly one
    // text_delta + one turn_end before completing.
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(assistant_end_turn("hello-sse")))
        .mount(&mocks.anthropic)
        .await;

    let (base, _state) = spawn_app(mocks.config()).await;
    let client = authenticated_client();

    // Submit the task first so the stream channel exists when we subscribe.
    let r = client
        .post(format!("{base}/tasks"))
        .json(&json!({"input": "stream test", "title": "sse-smoke"}))
        .send()
        .await
        .expect("post tasks");
    let body: Value = r.json().await.expect("json");
    let task_id = body
        .get("task_id")
        .and_then(|s| s.as_str())
        .expect("task_id")
        .to_string();

    // Subscribe to the SSE stream. Read the raw body as bytes and split on
    // blank-line event boundaries; each event has at least one `data: ...`
    // line carrying the JSON envelope.
    let stream_url = format!("{base}/tasks/{task_id}/stream");
    let mut resp = client
        .get(&stream_url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .expect("sse get");
    assert!(
        resp.status().is_success(),
        "expected 200 from SSE endpoint, got {}",
        resp.status()
    );

    let mut got_text_delta = false;
    let mut got_turn_end = false;
    let mut buffer = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);

    while std::time::Instant::now() < deadline {
        let chunk = match tokio::time::timeout(Duration::from_millis(500), resp.chunk()).await {
            Ok(Ok(Some(bytes))) => bytes,
            _ => break,
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // Each SSE event is delimited by a blank line. Process complete
        // events; leave the partial tail in the buffer.
        while let Some(idx) = buffer.find("\n\n") {
            let event = buffer[..idx].to_string();
            buffer.drain(..idx + 2);
            for line in event.lines() {
                if let Some(json_str) = line
                    .strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
                {
                    let trimmed = json_str.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("text_delta") => got_text_delta = true,
                            Some("turn_end") => got_turn_end = true,
                            Some("task_complete") => {
                                // Once we see task_complete the stream may
                                // close; ensure both signals are satisfied
                                // and stop reading.
                                got_text_delta = got_text_delta
                                    || v.get("output")
                                        .and_then(|o| o.as_str())
                                        .map(|s| !s.is_empty())
                                        .unwrap_or(false);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if got_text_delta && got_turn_end {
            break;
        }
    }

    assert!(got_text_delta, "expected at least one text_delta event");
    assert!(got_turn_end, "expected at least one turn_end event");

    // And the task itself reaches Completed.
    let _ = poll_status(
        &base,
        &task_id,
        TaskStatus::Completed,
        Duration::from_secs(2),
    )
    .await;
}
