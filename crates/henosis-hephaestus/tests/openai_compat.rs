//! Acceptance test for the OpenAI-compatible provider path.
//! Spins up a wiremock server impersonating an OpenAI-compatible endpoint
//! (the /chat/completions wire shape that ProxyProvider speaks). Verifies
//! that with `HEPHAESTUS_PROVIDER=openai-compat` and a configured base URL
//! Hephaestus drives the orchestrator through the proxy provider end to
//! end and reaches Completed.

use std::path::PathBuf;
use std::time::Duration;

use henosis_hephaestus::{Config, build_router, build_state, tasks::TaskStatus};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Service credential used by the isolated HTTP acceptance server.
const API_TOKEN: &str = "hephaestus-api-token-that-is-at-least-32-bytes";

/// Bundle of mocked external services used by the OpenAI-compat test.
/// Same coordination services as the Anthropic harness; only the LLM
/// endpoint shape differs.
struct Mocks {
    /// OpenAI-compatible LLM endpoint.
    openai: MockServer,
    /// Mocked Kleos (memory store, search, axon publish).
    kleos: MockServer,
    /// Mocked Chiasm (task create / output).
    chiasm: MockServer,
    /// Mocked Eidolon (gate check).
    eidolon: MockServer,
    /// Mocked Hermes (not exercised in this test but the config requires
    /// a URL).
    hermes: MockServer,
    /// Tempdir holding the dev credentials file the auth chain points at.
    /// Held across the test so the file outlives the request.
    _credfile: PathBuf,
    /// Temp directory owner; dropped last to keep the file alive.
    _tmp: TempDir,
}

/// Implements the behavior exposed by Mocks.
impl Mocks {
    /// Spin up all mocks and pre-populate the standard 200 responses for
    /// the coordination services. The LLM mock is configured per-test.
    async fn new() -> Self {
        let openai = MockServer::start().await;
        let kleos = MockServer::start().await;
        let chiasm = MockServer::start().await;
        let eidolon = MockServer::start().await;
        let hermes = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/store"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
            .mount(&kleos)
            .await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&kleos)
            .await;
        Mock::given(method("POST"))
            .and(path("/axon/publish"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&kleos)
            .await;
        Mock::given(method("POST"))
            .and(path("/tasks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 7})))
            .mount(&chiasm)
            .await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path_regex(r"^/tasks/\d+/output$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&chiasm)
            .await;
        Mock::given(method("POST"))
            .and(path("/gate/check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"allow": true})))
            .mount(&eidolon)
            .await;

        let tmp = tempfile::tempdir().expect("tempdir");
        let cred = tmp.path().join("credentials.json");
        tokio::fs::write(&cred, br#"{"claudeAiOauth":{"accessToken":"test-token"}}"#)
            .await
            .expect("cred write");

        Self {
            openai,
            kleos,
            chiasm,
            eidolon,
            hermes,
            _credfile: cred,
            _tmp: tmp,
        }
    }

    /// Build a Config wired for the OpenAI-compat provider. The provider URL
    /// points at the mock; the API key is set inline so the factory does
    /// not need phylaxd.
    fn config(&self) -> Config {
        Config {
            port: 0,
            env: henosis_hephaestus::config::DeployEnv::Dev,
            kleos_url: self.kleos.uri(),
            kleos_token_slot: "test/kleos".into(),
            chiasm_url: self.chiasm.uri(),
            chiasm_agent: "hephaestus".into(),
            chiasm_project: "openai-compat-test".into(),
            chiasm_token_slot: None,
            axon_url: self.kleos.uri(),
            plutus_url: None,
            dev_credentials_path: self._credfile.clone(),
            model: "gpt-4o-mini".into(),
            max_tokens: 256,
            http_timeout: Duration::from_secs(2),
            llm_timeout: Duration::from_secs(5),
            eidolon_url: self.eidolon.uri(),
            hermes_url: self.hermes.uri(),
            anthropic_url: format!("{}/v1/messages", self.openai.uri()),
            max_tool_turns: 6,
            sandbox_timeout: 5,
            sandbox_memory: "64m".into(),
            crucible_db: self._tmp.path().join("crucible.db"),
            crucible_enabled: false,
            cred_enabled: false,
            // The OpenAI-compat factory branch is what this test exercises.
            provider_kind: henosis_hephaestus::config::ProviderKind::OpenAiCompat,
            provider_url: Some(self.openai.uri()),
            provider_key_slot: None,
            provider_api_key: Some("sk-test".into()),
        }
    }
}

/// Spawn the Hephaestus router on a random local port and return its base
/// URL plus the AppState (so the test can inspect the StreamHub).
async fn spawn_app(cfg: Config) -> (String, henosis_hephaestus::AppState) {
    let state = build_state(cfg);
    let router = build_router(state.clone(), API_TOKEN.to_string());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
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

/// Poll GET /tasks/{id} until it reaches the target status or the timeout
/// expires. Returns the final TaskRecord JSON on success.
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

/// One-shot OpenAI chat completion that ends with stop_reason=stop. Mirrors
/// the OpenAI/Ollama/Azure chat-completions response shape that
/// synapse_provider::ProxyProvider parses.
fn openai_response_done(text: &str) -> Value {
    json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1700000000_u64,
        "model": "gpt-4o-mini",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
    })
}

/// End-to-end happy path: submit a task, the OpenAI-compat provider returns
/// a single end_turn response, and the task reaches Completed with the
/// expected output. Verifies that the provider factory + ProxyProvider
/// path is fully wired.
#[tokio::test]
async fn openai_compat_happy_path() {
    let mocks = Mocks::new().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_response_done("openai-ok")))
        .mount(&mocks.openai)
        .await;

    let (base, _state) = spawn_app(mocks.config()).await;
    let client = authenticated_client();

    let r = client
        .post(format!("{base}/tasks"))
        .json(&json!({"input": "say hi via openai", "title": "openai-smoke"}))
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
        Some("openai-ok")
    );

    // Confirm the provider hit /chat/completions at least once -- proves
    // the factory routed through ProxyProvider rather than the Anthropic
    // path.
    let openai_hits = mocks
        .openai
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.url.path() == "/chat/completions")
        .count();
    assert!(openai_hits >= 1, "expected /chat/completions to be hit");

    // Ensure the request carried the bearer token configured via
    // HEPHAESTUS_PROVIDER_KEY.
    let saw_bearer = mocks
        .openai
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .any(|r| {
            r.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap_or("").contains("sk-test"))
                .unwrap_or(false)
        });
    assert!(saw_bearer, "expected bearer auth header with sk-test");
}
