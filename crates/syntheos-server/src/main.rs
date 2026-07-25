//! `syntheos-server` binary: the single entry point that boots and serves Henosis.

use std::collections::BTreeSet;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ed25519_dalek::{SigningKey, VerifyingKey};
use henosis_approval::ApprovalStore;
use henosis_audit::{AuditStore, OriginSigner, WitnessClient, WitnessedAudit};
use henosis_broca::BrocaStore;
use henosis_chiasm::ChiasmStore;
use henosis_eidolon::supervisor::{self, Supervisor, SupervisorConfig};
use henosis_eidolon::{EidolonOutputFilter, EidolonPolicy};
use henosis_loom::{
    CompositeStepExecutor, HephaestusDispatch, HephaestusStepExecutor, LoomStore, TransformExecutor,
};
use henosis_pistis::crypto::SecretKey as PistisSecretKey;
use henosis_pistis::{
    ActionKind, AdmittedPrincipal, Capability, InMemoryRoomStateSource, RoomPolicy, RoomScope,
    RoomState, RoomStateSource, RoomTrustStore,
};
use henosis_plutus::{LocalPolicyBackend, PlutusStore, PolicyBackend, QuotaTier, Role};
use henosis_soma::SomaStore;
use henosis_thymus::ThymusStore;
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalKind, TenantId};
use syntheos_dispatch::Dispatcher;
use syntheos_identity::{PrincipalDirectory, SqliteDirectory};
use syntheos_server::authority::{AuditBoundary, AuditExecutionGuard, AuthorityState};
use syntheos_server::billing::BillingState;
use syntheos_server::cli::{CliPaths, CliRunner, Command, HttpControlApi, InitMode, RunResult};
use syntheos_server::operator::OperatorState;
use syntheos_server::{
    public_gate_chain, runtime_router, spawn_action_reactor, AppState, HenosisExecutor,
    SomaQualitySink,
};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing_subscriber::EnvFilter;
use zeroize::{Zeroize, Zeroizing};

/// Largest request body the server accepts, in bytes (1 MiB).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Bridges the Loom [`HephaestusDispatch`] seam to the in-process Hephaestus executor.
///
/// Constructed once at server boot from `henosis_hephaestus::Config::from_env()` and held
/// inside a [`HephaestusStepExecutor`] that is composed with [`TransformExecutor`] into the
/// [`CompositeStepExecutor`] attached to the [`LoomStore`].
///
/// When a Loom workflow step of type `hephaestus` runs, [`Self::run`] is called with the
/// step's merged input (run input overlaid with dep outputs, then overlaid with step config).
/// The payload is mapped to a [`henosis_hephaestus::CreateTaskBody`] and forwarded to
/// [`henosis_hephaestus::run_task_to_completion`], which blocks until the agent loop finishes.
///
/// If hephaestus is not configured (e.g. no `HEPHAESTUS_ANTHROPIC_TOKEN` / provider creds),
/// the AppState still constructs successfully but individual task executions will fail with
/// a provider-auth error, which propagates back as `Err(message)` and burns the step's
/// retry budget with a meaningful error. This is explicitly NOT a silent stub -- the error
/// message identifies the misconfiguration rather than pretending the step succeeded.
struct HephaestusRuntimeDispatch {
    /// The hephaestus application state: clients (auth, LLM, Hermes, Kleos), task store, SSE hub.
    state: henosis_hephaestus::AppState,
}

#[async_trait]
/// Execute Loom Hephaestus steps against the in-process runtime.
impl HephaestusDispatch for HephaestusRuntimeDispatch {
    /// Forward the step payload to the in-process Hephaestus executor and await the result.
    ///
    /// Extracts `"input"` from the payload as the agent task prompt (falling back to the
    /// serialized payload if absent) and optional fields `agent`, `project`, `title`,
    /// `tenant_id`, `system`, `verify_command` from the same object. Maps the terminal
    /// [`henosis_hephaestus::TaskRecord`] to a JSON output on success or an error string on failure.
    async fn run(&self, input: serde_json::Value) -> Result<serde_json::Value, String> {
        // Extract the agent prompt: prefer the "input" string key so step config can pin it.
        let prompt = input
            .get("input")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| input.to_string());

        /// Extract an optional string field from a JSON object.
        fn opt_str(v: &serde_json::Value, key: &str) -> Option<String> {
            v.get(key).and_then(|s| s.as_str()).map(String::from)
        }

        let body = henosis_hephaestus::CreateTaskBody {
            input: prompt,
            agent: opt_str(&input, "agent"),
            project: opt_str(&input, "project"),
            title: opt_str(&input, "title"),
            tenant_id: opt_str(&input, "tenant_id"),
            system: opt_str(&input, "system"),
            verify_command: opt_str(&input, "verify_command"),
        };

        let record = henosis_hephaestus::run_task_to_completion(self.state.clone(), body)
            .await
            .map_err(|e| format!("hephaestus pre-flight: {e}"))?;

        match record.status {
            henosis_hephaestus::TaskStatus::Completed => Ok(serde_json::json!({
                "task_id": record.id,
                "status": "completed",
                "output": record.output.unwrap_or_default(),
            })),
            henosis_hephaestus::TaskStatus::Failed => Err(format!(
                "hephaestus task {} failed: {}",
                record.id,
                record.error.unwrap_or_else(|| "unknown".to_string())
            )),
            other => Err(format!(
                "hephaestus task {} in unexpected state: {:?}",
                record.id, other
            )),
        }
    }
}

/// How long a single request may run before the server answers `408 Request Timeout`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum requests in flight at once across the whole surface; excess connections queue on the
/// shared semaphore instead of piling onto the runtime.
const MAX_IN_FLIGHT: usize = 1024;

/// Default maximum seconds for acquiring a Plutus Postgres connection.
const DEFAULT_PLUTUS_ACQUIRE_TIMEOUT_SECS: u64 = 10;

/// Largest accepted Plutus acquisition timeout, preventing an accidental unbounded boot stall.
const MAX_PLUTUS_ACQUIRE_TIMEOUT_SECS: u64 = 300;

/// Default seconds between production Loom timeout-enforcement passes.
const DEFAULT_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS: u64 = 30;

/// Largest accepted Loom timeout sweep interval, keeping enforcement operationally prompt.
const MAX_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS: u64 = 300;

/// Largest local authority credential file read into memory.
const MAX_AUTHORITY_FILE_BYTES: u64 = 4096;

/// Largest supervisor rules document accepted during startup.
const MAX_SUPERVISOR_RULES_FILE_BYTES: u64 = 1024 * 1024;

/// Stable synthetic room identifier used only by explicit loopback local policy.
const LOCAL_PISTIS_ROOM_ID: &str = "!henosis-local:loopback";

/// Initial signed generation for the ephemeral loopback Pistis room.
const LOCAL_PISTIS_ROOM_GENERATION: u64 = 1;

/// Validated identity pair for the explicit loopback-only policy backend.
#[derive(Debug)]
struct LocalPolicyConfig {
    /// Tenant recognized by the local policy authority.
    tenant: TenantId,
    /// Principal granted owner membership in the local tenant.
    principal: syntheos_contracts::PrincipalId,
}

/// Parse local commands and load private configuration before creating worker threads.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli_paths = CliPaths::from_environment()?;
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if !arguments.is_empty() {
        let command = Command::parse(&arguments)?;
        let result = if control_command(&command) {
            syntheos_server::cli::load_local_environment_if_present(&cli_paths)?;
            let control_api = HttpControlApi::from_environment()?;
            CliRunner::local(cli_paths.clone())
                .with_control_api(&control_api)
                .run(command)?
        } else {
            CliRunner::local(cli_paths.clone()).run(command)?
        };
        if !matches!(result, RunResult::Serve) {
            println!("{}", result.render()?);
            return Ok(());
        }
    }
    if auto_init_requested(optional_env("HENOSIS_AUTO_INIT")?.as_deref())? {
        CliRunner::local(cli_paths.clone()).run(Command::Init(InitMode::Quick))?;
    }
    syntheos_server::cli::load_local_environment_if_present(&cli_paths)?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_server())
}

/// Identify commands that need the authenticated live control-plane client.
fn control_command(command: &Command) -> bool {
    matches!(
        command,
        Command::Status
            | Command::Update
            | Command::Uninstall
            | Command::Token(_)
            | Command::Approvals(_)
            | Command::AuditVerify
    )
}

/// Validate the explicit container-oriented quick-initialization switch.
fn auto_init_requested(value: Option<&str>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some("quick") => Ok(true),
        Some(_) => Err("HENOSIS_AUTO_INIT must be exactly quick when enabled".to_string()),
    }
}

/// Initialize every kernel authority and serve the unified Syntheos API.
async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Resolve the explicit policy authority before selecting security boundaries. Listen topology
    // never decides whether production witness and broker authentication requirements apply.
    let raw_addr = std::env::var("SYNTHEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8088".to_string());
    let addr = validated_bind_addr(&raw_addr)?;
    let loom_timeout_sweep_interval = loom_timeout_sweep_interval_from_env()?;
    let plutus_url = optional_env("SYNTHEOS_PLUTUS_DB")?;
    let local_config = validated_local_policy_config(
        optional_env("SYNTHEOS_LOCAL_POLICY")?.as_deref(),
        plutus_url.as_deref(),
        addr,
        optional_env("SYNTHEOS_PLUTUS_OPERATOR_TENANT")?.as_deref(),
        optional_env("SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL")?.as_deref(),
    )?;
    let local_mode = local_config.is_some();
    validate_phylaxd_config(local_mode)?;

    // Open exactly one real Plutus policy backend before any dependent local state. Production
    // uses PostgreSQL. An explicit local install uses a single-operator backend with real RBAC,
    // quota, and rate-limit checks.
    let (plutus, plutus_store): (Arc<dyn PolicyBackend>, Option<Arc<PlutusStore>>) =
        if let Some(config) = local_config.as_ref() {
            tracing::info!(
                tenant = %config.tenant,
                principal = %config.principal,
                "local Plutus policy authority enabled (single operator, loopback only)"
            );
            (
                Arc::new(LocalPolicyBackend::new(
                    config.tenant,
                    config.principal,
                    Role::Owner,
                    QuotaTier::Free,
                )),
                None,
            )
        } else {
            let plutus_url = plutus_url
                .ok_or("SYNTHEOS_PLUTUS_DB is required unless SYNTHEOS_LOCAL_POLICY=1")?;
            let plutus_acquire_timeout = plutus_acquire_timeout_from_env()?;
            let store = Arc::new(
                PlutusStore::open_with_acquire_timeout(&plutus_url, plutus_acquire_timeout)
                    .await
                    .map_err(|error| format!("plutus store open failed: {error}"))?,
            );
            tracing::info!(
                url = %redact_postgres_password(&plutus_url),
                "PostgreSQL Plutus policy authority open"
            );
            (store.clone(), Some(store))
        };

    // Wire the remaining foundation: bus and directory first, stores next, then the dispatcher (its
    // eidolon gate reads drift from the Thymus store, so Thymus must exist first).
    // The directory is the persistent SqliteDirectory (G2): with persistent Chiasm/Soma stores,
    // an in-memory directory would orphan every projection row on restart.
    let bus = Arc::new(AxonBus::new());
    let identity_db = db_path("SYNTHEOS_IDENTITY_DB", "data/identity.sqlite")?;
    // Keep a concrete Arc<SqliteDirectory> so the operator bootstrap and OperatorState
    // can call the accounts API (create_account, get_account, verify_login) which are
    // defined on SqliteDirectory directly, not on the PrincipalDirectory trait.
    let directory_store = Arc::new(SqliteDirectory::open(&identity_db)?);
    let directory: Arc<dyn PrincipalDirectory> = directory_store.clone();
    tracing::info!(path = %identity_db, "principal directory open");
    let approval_db = db_path("HENOSIS_APPROVAL_DB", "data/approval.sqlite")?;
    let approvals = Arc::new(ApprovalStore::open(&approval_db)?);
    tracing::info!(path = %approval_db, "durable approval store open");
    let audit = audit_boundary_from_env(local_mode)?;
    tracing::info!(
        witnessed = audit.is_witnessed(),
        "durable audit boundary open"
    );

    // Kernel services open persistent SQLite stores at configurable paths and apply migrations.
    let chiasm_db = db_path("SYNTHEOS_CHIASM_DB", "data/chiasm.sqlite")?;
    let chiasm = Arc::new(ChiasmStore::open(&chiasm_db, bus.clone())?);
    tracing::info!(path = %chiasm_db, "chiasm task store open");
    let soma_db = db_path("SYNTHEOS_SOMA_DB", "data/soma.sqlite")?;
    let soma = Arc::new(SomaStore::open(&soma_db, bus.clone(), directory.clone())?);
    tracing::info!(path = %soma_db, "soma presence store open");
    // Without an attached narrator, Broca uses its deterministic template path.
    let broca_db = db_path("SYNTHEOS_BROCA_DB", "data/broca.sqlite")?;
    let broca = Arc::new(BrocaStore::open(&broca_db, bus.clone())?);
    tracing::info!(path = %broca_db, "broca narration log open");
    // Build the in-process Hephaestus executor from env (Config::from_env). If provider
    // credentials are absent the AppState still constructs and attaches; individual task
    // executions will fail with a meaningful auth error rather than silently succeeding.
    let heph_state = henosis_hephaestus::build_state(henosis_hephaestus::Config::from_env());
    let heph_dispatch = HephaestusRuntimeDispatch { state: heph_state };

    // CompositeStepExecutor handles pure-JSON transforms inline and dispatches agent tasks to
    // Hephaestus. First-match wins; unclaimed types
    // (action, decision, wait, ...) stay Running for external completion via complete_step.
    let loom_db = db_path("SYNTHEOS_LOOM_DB", "data/loom.sqlite")?;
    let loom = Arc::new(
        LoomStore::open(&loom_db, bus.clone())?.with_executor(Box::new(
            CompositeStepExecutor::new(vec![
                Box::new(TransformExecutor),
                Box::new(HephaestusStepExecutor::new(heph_dispatch)),
            ]),
        )),
    );
    tracing::info!(path = %loom_db, "loom workflow engine open (composite executor: transform + hephaestus)");
    // Evaluations and drift propagate into the agents' Soma presence via the sink adapter.
    let thymus_db = db_path("SYNTHEOS_THYMUS_DB", "data/thymus.sqlite")?;
    let thymus = Arc::new(
        ThymusStore::open(&thymus_db, bus.clone())?
            .with_quality_sink(Box::new(SomaQualitySink(soma.clone()))),
    );
    tracing::info!(path = %thymus_db, "thymus quality store open");

    if let Some(config) = local_config.as_ref() {
        bootstrap_local_machine_token(&directory_store, config)?;
    }

    // Subscribe before the dispatcher is made reachable so no action can race past the two
    // downstream projections required by the kernel definition of done.
    let _action_reactor = spawn_action_reactor(bus.clone(), chiasm.clone(), broca.clone());

    // Explicit loopback local policy receives one signed compatibility room for the documented
    // probe. Production keeps an empty source and trust store so capability requests fail closed
    // until a deployment supplies trusted Pistis room state.
    let (pistis_source, pistis_trust) = pistis_authority_from_local_policy(local_config.as_ref())?;

    let policy = EidolonPolicy::default();
    let execution_guard = Arc::new(AuditExecutionGuard::new(approvals.clone(), audit.clone()));
    let dispatcher = Arc::new(
        Dispatcher::new(
            public_gate_chain(
                &policy,
                thymus.clone(),
                pistis_source,
                pistis_trust,
                approvals.clone(),
                plutus.clone(),
            )?,
            Box::new(HenosisExecutor::from_env()),
            bus.clone(),
        )?
        .with_output_filter(Box::new(EidolonOutputFilter::new(&policy)?))
        .with_execution_guard(execution_guard),
    );

    // The Eidolon supervisor watches session JSONL and publishes violation events on the shared
    // bus. It runs only when both the watch directory and event identity are configured, because
    // a fabricated identity would poison the audit trail.
    match supervisor_from_env(bus.clone()) {
        Ok(Some(sup)) => {
            tokio::spawn(sup.run());
            tracing::info!("eidolon supervisor task started");
        }
        Ok(None) => {
            tracing::info!("eidolon supervisor disabled (SYNTHEOS_SUPERVISOR_WATCH_DIR unset)");
        }
        Err(err) => return Err(err),
    }

    // The feature-gated cognitive core uses a lightweight session without an embedder or
    // background loops. It opens a persistent path-backed store at `SYNTHEOS_COGNITION_DB`
    // (default `data/cognition.db`) for the `/cognition/memory*` routes. The parent directory
    // is created on boot. See scripts/known-incomplete.md for the remaining facade limits.
    #[cfg(feature = "cognition")]
    let cognition = {
        let db_path = std::env::var("SYNTHEOS_COGNITION_DB")
            .unwrap_or_else(|_| "data/cognition.db".to_string());
        if let Some(parent) = std::path::Path::new(&db_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let cog = Arc::new(henosis_cognition::Cognition::open_path(&db_path).await?);
        tracing::info!(%db_path, "cognition core open (persistent lite session; /cognition/memory* live)");
        cog
    };

    let mut state = AppState::new(
        dispatcher.clone(),
        directory,
        bus.clone(),
        chiasm.clone(),
        soma.clone(),
        broca.clone(),
        loom.clone(),
        thymus.clone(),
        #[cfg(feature = "cognition")]
        cognition,
    );

    // The authenticated public API and operator surface share one verified signing key. Public
    // runtime startup never silently drops either authentication boundary.
    let op_state = operator_state_from_env(
        directory_store.clone(),
        plutus.clone(),
        soma.clone(),
        chiasm.clone(),
        broca.clone(),
        thymus.clone(),
        loom.clone(),
        bus.clone(),
    )
    .await?
    .ok_or("SYNTHEOS_OPERATOR_JWT_SECRET is required")?;
    if let Some(store) = plutus_store.as_deref() {
        if operator_bootstrap_requested()? {
            bootstrap_operator_if_configured(&directory_store, store).await?;
        }
        if !directory_store.has_operator_accounts()? {
            return Err(
                "fresh production startup requires SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL and SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD"
                    .into(),
            );
        }
    }
    let authority = AuthorityState {
        dispatcher,
        accounts: directory_store.clone(),
        policy: plutus.clone(),
        jwt_secret: op_state.jwt_secret.clone(),
        approvals,
        audit,
    };
    state = state.with_operator(op_state).with_authority(authority);
    tracing::info!(
        "authenticated operator and authority surfaces enabled: /api/auth/*, /api/v1/*, /ws"
    );

    // Mount the Stripe billing webhook when SYNTHEOS_STRIPE_WEBHOOK_SECRET is configured.
    // A set-but-empty secret is a hard boot error so misconfiguration is never silent.
    if let Some(billing_state) = billing_state_from_env(plutus_store.clone())? {
        state = state.with_billing(billing_state);
        tracing::info!("stripe billing webhook enabled: POST /billing/stripe/webhook");
    }

    // Resource limits around the whole surface: cap the body size, time out slow requests, and
    // bound how many run concurrently.
    let app = runtime_router(state, local_mode)
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "henosis listening with authenticated authority, phylaxd broker, and durable audit");

    let (loom_sweeper_shutdown, loom_sweeper_shutdown_rx) = tokio::sync::watch::channel(false);
    let mut loom_sweeper = tokio::spawn(run_loom_timeout_sweeper(
        loom,
        loom_timeout_sweep_interval,
        loom_sweeper_shutdown_rx,
    ));
    let mut server = Box::pin(
        axum::serve(listener, app)
            .with_graceful_shutdown({
                let mut requested_shutdown = loom_sweeper_shutdown.subscribe();
                async move {
                    tokio::select! {
                        _ = shutdown_signal() => {}
                        _ = requested_shutdown.changed() => {}
                    }
                }
            })
            .into_future(),
    );

    tokio::select! {
        server_result = &mut server => {
            loom_sweeper_shutdown.send_replace(true);
            let sweeper_result = loom_sweeper.await;
            server_result?;
            sweeper_result
                .map_err(|error| format!("Loom timeout sweeper task failed: {error}"))?;
            Ok(())
        }
        sweeper_result = &mut loom_sweeper => {
            loom_sweeper_shutdown.send_replace(true);
            let server_result = server.await;
            server_result?;
            match sweeper_result {
                Ok(()) => Err("Loom timeout sweeper exited before server shutdown".into()),
                Err(error) => Err(format!("Loom timeout sweeper task failed: {error}").into()),
            }
        }
    }
}

/// Parse the configured IP socket address without accepting ambiguous hostnames.
fn validated_bind_addr(raw: &str) -> Result<SocketAddr, String> {
    let addr = raw.parse::<SocketAddr>().map_err(|_| {
        "SYNTHEOS_ADDR must be an IP socket address such as 127.0.0.1:8088".to_string()
    })?;
    Ok(addr)
}

/// Read an optional Unicode environment value without treating malformed data as absent.
fn optional_env(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name}: {error}")),
    }
}

/// Validate the explicit local policy selection and its single-operator identity.
fn validated_local_policy_config(
    local_policy: Option<&str>,
    postgres_url: Option<&str>,
    addr: SocketAddr,
    tenant: Option<&str>,
    principal: Option<&str>,
) -> Result<Option<LocalPolicyConfig>, String> {
    match local_policy {
        None => return Ok(None),
        Some("1") => {}
        Some(_) => return Err("SYNTHEOS_LOCAL_POLICY must be exactly 1 when enabled".to_string()),
    }
    if postgres_url.is_some_and(|value| !value.is_empty()) {
        return Err(
            "SYNTHEOS_LOCAL_POLICY and SYNTHEOS_PLUTUS_DB are mutually exclusive".to_string(),
        );
    }
    if !addr.ip().is_loopback() {
        return Err("local policy mode requires a loopback SYNTHEOS_ADDR".to_string());
    }
    let tenant = tenant
        .ok_or("SYNTHEOS_PLUTUS_OPERATOR_TENANT is required in local policy mode")?
        .parse::<TenantId>()
        .map_err(|error| format!("SYNTHEOS_PLUTUS_OPERATOR_TENANT: {error}"))?;
    let principal = principal
        .ok_or("SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL is required in local policy mode")?
        .parse::<syntheos_contracts::PrincipalId>()
        .map_err(|error| format!("SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL: {error}"))?;
    Ok(Some(LocalPolicyConfig { tenant, principal }))
}

/// Build the ephemeral signed Pistis room only from validated loopback local policy.
fn pistis_authority_from_local_policy(
    config: Option<&LocalPolicyConfig>,
) -> Result<(Arc<dyn RoomStateSource>, Arc<RoomTrustStore>), henosis_pistis::PistisError> {
    let mut source = InMemoryRoomStateSource::new();
    let mut trust = RoomTrustStore::new();
    if let Some(config) = config {
        let scope = RoomScope::new(config.tenant, LOCAL_PISTIS_ROOM_ID);
        let (_, issuer_key) = PistisSecretKey::generate();
        let (_, room_root_key) = PistisSecretKey::generate();
        let (_, principal_key) = PistisSecretKey::generate();
        let admission = AdmittedPrincipal::new(
            scope.clone(),
            config.principal,
            principal_key.public_key(),
            &room_root_key,
            vec![Capability {
                name: "henosis".to_string(),
                action_kinds: BTreeSet::from([ActionKind::Message]),
                granted_by: "local-policy".to_string(),
                expires_at: None,
            }],
        );
        let state = RoomState::from_genesis(
            scope.clone(),
            LOCAL_PISTIS_ROOM_GENERATION,
            RoomPolicy::default(),
            BTreeSet::from([room_root_key.public_key()]),
            &issuer_key,
            vec![admission],
        )?;
        trust.pin(scope, issuer_key.public_key(), LOCAL_PISTIS_ROOM_GENERATION)?;
        source.insert(state);
    }
    Ok((Arc::new(source), Arc::new(trust)))
}

/// Validate the outbound credential-broker URL and mode-specific authentication boundary.
fn validate_phylaxd_config(local_mode: bool) -> Result<(), String> {
    let config = henosis_hermes::config::Config::from_env();
    let url = reqwest::Url::parse(&config.phylaxd_url)
        .map_err(|_| "PHYLAXD_URL must be an absolute HTTP or HTTPS URL".to_string())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("PHYLAXD_URL must not contain credentials, a query, or a fragment".to_string());
    }
    let loopback_host = is_loopback_url_host(url.host_str().unwrap_or_default());
    let secure_transport = url.scheme() == "https";
    if !(secure_transport || url.scheme() == "http" && loopback_host) {
        return Err("PHYLAXD_URL must use HTTPS unless it targets loopback".to_string());
    }
    match config.phylaxd_token.as_deref() {
        Some(token)
            if token.len() >= 32
                && token.trim() == token
                && !token.chars().any(char::is_whitespace) => {}
        Some(_) => {
            return Err(
                "HERMES_PHYLAXD_TOKEN must contain at least 32 non-whitespace bytes".to_string(),
            );
        }
        None if !local_mode => {
            return Err("production deployments require HERMES_PHYLAXD_TOKEN".to_string());
        }
        None => {}
    }
    Ok(())
}

/// Determine whether a serialized URL host identifies the local machine.
fn is_loopback_url_host(host: &str) -> bool {
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    address.eq_ignore_ascii_case("localhost")
        || address
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Open local audit storage and require an independent witness outside explicit local mode.
fn audit_boundary_from_env(local_mode: bool) -> Result<AuditBoundary, Box<dyn std::error::Error>> {
    let audit_path = db_path("HENOSIS_AUDIT_DB", "data/audit.sqlite")?;
    let store = AuditStore::open(&audit_path)?;
    let witness_names = [
        "HENOSIS_WITNESS_URL",
        "HENOSIS_AUDIT_ORIGIN_KEY_FILE",
        "HENOSIS_AUDIT_ORIGIN_KEY_ID",
        "HENOSIS_WITNESS_PUBLIC_KEY_FILE",
        "HENOSIS_WITNESS_KEY_ID",
    ];
    let configured = witness_names
        .iter()
        .filter(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
        .count();
    if configured == 0 && local_mode {
        return Ok(AuditBoundary::Local(store));
    }
    if configured != witness_names.len() {
        return Err(format!(
            "witnessed audit requires all of: {}",
            witness_names.join(", ")
        )
        .into());
    }
    require_witness_file_security()?;

    let witness_url = required_nonempty_env("HENOSIS_WITNESS_URL")?;
    let origin_key_path = required_nonempty_env("HENOSIS_AUDIT_ORIGIN_KEY_FILE")?;
    let origin_key_id = required_nonempty_env("HENOSIS_AUDIT_ORIGIN_KEY_ID")?;
    let witness_key_path = required_nonempty_env("HENOSIS_WITNESS_PUBLIC_KEY_FILE")?;
    let witness_key_id = required_nonempty_env("HENOSIS_WITNESS_KEY_ID")?;
    let origin = OriginSigner::new(
        origin_key_id,
        load_signing_key(Path::new(&origin_key_path))?,
    )?;
    let witness = WitnessClient::new(
        &witness_url,
        witness_key_id,
        load_verifying_key(Path::new(&witness_key_path))?,
        Duration::from_secs(5),
    )?;
    Ok(AuditBoundary::Witnessed(Box::new(WitnessedAudit::new(
        store, origin, witness,
    ))))
}

/// Read one mandatory non-blank environment value without logging its contents.
fn required_nonempty_env(name: &str) -> Result<String, Box<dyn std::error::Error>> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(format!("{name} is required").into()),
    }
}

/// Load a base64-encoded Ed25519 signing key from an owner-private regular file.
fn load_signing_key(path: &Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let encoded = Zeroizing::new(read_owned_regular_text(
        path,
        0o077,
        "audit origin signing key",
    )?);
    let decoded = Zeroizing::new(BASE64.decode(encoded.trim())?);
    let key_bytes = Zeroizing::new(decoded.as_slice().try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "audit origin signing key must contain 32 bytes",
        )
    })?);
    let key = SigningKey::from_bytes(&key_bytes);
    Ok(key)
}

/// Load a base64-encoded Ed25519 verifying key from an integrity-protected regular file.
fn load_verifying_key(path: &Path) -> Result<VerifyingKey, Box<dyn std::error::Error>> {
    let encoded = Zeroizing::new(read_owned_regular_text(path, 0o022, "witness public key")?);
    let bytes = Zeroizing::new(BASE64.decode(encoded.trim())?);
    let key: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "witness public key must contain 32 bytes",
        )
    })?;
    Ok(VerifyingKey::from_bytes(&key)?)
}

/// Open a local operator-selected file with platform-native leaf and special-file defenses.
#[cfg(any(unix, windows))]
fn open_regular_readonly(
    path: &Path,
    label: &str,
) -> Result<std::fs::File, Box<dyn std::error::Error>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        };

        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(format!("{label} must be a regular file").into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileType, FILE_ATTRIBUTE_REPARSE_POINT, FILE_TYPE_DISK,
        };

        // SAFETY: the raw handle remains owned and valid for the duration of this call.
        let file_type = unsafe { GetFileType(file.as_raw_handle()) };
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || file_type != FILE_TYPE_DISK
        {
            return Err(format!("{label} must be a non-reparse local disk file").into());
        }
    }
    Ok(file)
}

/// Fail closed where the runtime has no descriptor-level no-follow implementation.
#[cfg(not(any(unix, windows)))]
fn open_regular_readonly(
    _path: &Path,
    label: &str,
) -> Result<std::fs::File, Box<dyn std::error::Error>> {
    Err(format!("{label} cannot be opened safely on this platform").into())
}

/// Open, validate, and read one bounded authority file through the same descriptor.
#[cfg(unix)]
fn read_owned_regular_text(
    path: &Path,
    forbidden_mode: u32,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let file = open_regular_readonly(path, label)?;
    let metadata = file.metadata()?;
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_file()
        || metadata.uid() != effective_uid
        || metadata.permissions().mode() & forbidden_mode != 0
    {
        return Err(format!("{label} has unsafe ownership or permissions").into());
    }
    if metadata.len() > MAX_AUTHORITY_FILE_BYTES {
        return Err(format!("{label} exceeds the maximum size").into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_AUTHORITY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_AUTHORITY_FILE_BYTES {
        bytes.zeroize();
        return Err(format!("{label} exceeds the maximum size").into());
    }
    authority_text(bytes, label)
}

/// Open and read one bounded regular file on platforms without Unix ownership metadata.
#[cfg(not(unix))]
fn read_owned_regular_text(
    path: &Path,
    _forbidden_mode: u32,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Read;

    let file = open_regular_readonly(path, label)?;
    let metadata = file.metadata()?;
    if metadata.len() > MAX_AUTHORITY_FILE_BYTES {
        return Err(format!("{label} exceeds the maximum size").into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_AUTHORITY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_AUTHORITY_FILE_BYTES {
        bytes.zeroize();
        return Err(format!("{label} exceeds the maximum size").into());
    }
    authority_text(bytes, label)
}

/// Decode bounded authority bytes as UTF-8 and clear the intermediate buffer.
fn authority_text(mut bytes: Vec<u8>, label: &str) -> Result<String, Box<dyn std::error::Error>> {
    let text = std::str::from_utf8(&bytes)
        .map(str::to_owned)
        .map_err(|_| format!("{label} must contain valid UTF-8"));
    bytes.zeroize();
    text.map_err(Into::into)
}

/// Open and read one bounded regular text file through a validated descriptor.
fn read_bounded_regular_text(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    use std::io::Read;

    let file = open_regular_readonly(path, label)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes {
        return Err(format!("{label} exceeds the maximum size").into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(format!("{label} exceeds the maximum size").into());
    }
    authority_text(bytes, label)
}

/// Confirm that witnessed-audit key permissions can be enforced on this platform.
#[cfg(unix)]
fn require_witness_file_security() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}

/// Refuse witnessed audit where file ownership and mode enforcement are unavailable.
#[cfg(not(unix))]
fn require_witness_file_security() -> Result<(), Box<dyn std::error::Error>> {
    Err("witnessed audit key loading requires Unix file ownership enforcement".into())
}

/// Ensure a local installation has one usable owner token without printing its credential.
fn bootstrap_local_machine_token(
    directory: &SqliteDirectory,
    config: &LocalPolicyConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let token_path = std::env::var("HENOSIS_LOCAL_TOKEN_FILE")
        .unwrap_or_else(|_| "data/local-operator.token".to_string());
    let path = Path::new(&token_path);
    let now = chrono::Utc::now().timestamp();
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let mut token = read_owned_regular_text(path, 0o077, "local operator token")?;
            let authenticated = directory.authenticate_machine_token(token.trim(), now)?;
            token.zeroize();
            let metadata = authenticated.ok_or("local operator token is invalid or revoked")?;
            if metadata.tenant != config.tenant
                || metadata.principal != config.principal
                || !metadata.scopes.iter().any(|scope| scope == "admin")
                || !metadata.scopes.iter().any(|scope| scope == "dispatch")
            {
                return Err("local operator token does not match the configured owner".into());
            }
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
        set_private_directory_mode(parent)?;
    }
    let issued = directory.create_machine_token(
        config.tenant,
        config.principal,
        "local-owner",
        vec![
            "admin".to_string(),
            "audit:read".to_string(),
            "dispatch".to_string(),
        ],
        None,
        now,
    )?;
    if let Err(error) = write_private_new_file(path, issued.token.as_bytes()) {
        let _ = directory.revoke_machine_token(config.tenant, issued.metadata.id, now);
        return Err(error.into());
    }
    tracing::info!(path = %path.display(), "local operator token created");
    Ok(())
}

/// Create one new owner-private file without following or replacing an existing path.
fn write_private_new_file(path: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// Apply owner-only permissions to a local state directory.
#[cfg(unix)]
fn set_private_directory_mode(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Keep local state initialization portable on platforms without Unix modes.
#[cfg(not(unix))]
fn set_private_directory_mode(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

/// Load a bounded supervisor rule set from a stable regular file.
fn load_supervisor_rules(path: &Path) -> Result<Vec<supervisor::Rule>, Box<dyn std::error::Error>> {
    let content =
        read_bounded_regular_text(path, MAX_SUPERVISOR_RULES_FILE_BYTES, "supervisor rules")?;
    Ok(supervisor::rules_from_json(&content)?)
}

/// Build the supervisor from the environment, when enabled.
///
/// `SYNTHEOS_SUPERVISOR_WATCH_DIR` unset = disabled (`Ok(None)`). When set, the identity the
/// violation events carry is required (`SYNTHEOS_SUPERVISOR_TENANT` /
/// `SYNTHEOS_SUPERVISOR_PRINCIPAL`, canonical UUID strings) and a configured-but-unreadable
/// rules file (`SYNTHEOS_SUPERVISOR_RULES`) is a boot error rather than a silent fallback to
/// defaults. `SYNTHEOS_SUPERVISOR_ALLOWED_PATHS` enables the colon-separated file-scope check.
fn supervisor_from_env(
    bus: Arc<AxonBus>,
) -> Result<Option<Supervisor>, Box<dyn std::error::Error>> {
    let watch_dir = match std::env::var("SYNTHEOS_SUPERVISOR_WATCH_DIR") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => return Ok(None),
    };
    let tenant = std::env::var("SYNTHEOS_SUPERVISOR_TENANT")
        .map_err(|_| "SYNTHEOS_SUPERVISOR_TENANT is required when the supervisor is enabled")?
        .parse()
        .map_err(|e| format!("SYNTHEOS_SUPERVISOR_TENANT: {e}"))?;
    let principal = std::env::var("SYNTHEOS_SUPERVISOR_PRINCIPAL")
        .map_err(|_| "SYNTHEOS_SUPERVISOR_PRINCIPAL is required when the supervisor is enabled")?
        .parse()
        .map_err(|e| format!("SYNTHEOS_SUPERVISOR_PRINCIPAL: {e}"))?;
    let rules = match std::env::var("SYNTHEOS_SUPERVISOR_RULES") {
        Ok(path) if !path.is_empty() => load_supervisor_rules(Path::new(&path))
            .map_err(|e| format!("SYNTHEOS_SUPERVISOR_RULES {path:?}: {e}"))?,
        _ => supervisor::default_rules(),
    };
    let allowed_paths: Vec<String> = std::env::var("SYNTHEOS_SUPERVISOR_ALLOWED_PATHS")
        .map(|v| {
            v.split(':')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        watch_dir = %watch_dir.display(),
        rules = rules.len(),
        scope_check = !allowed_paths.is_empty(),
        "eidolon supervisor configured"
    );
    Ok(Some(Supervisor::new(
        SupervisorConfig {
            watch_dir,
            rules,
            allowed_paths,
            tenant,
            principal,
        },
        bus,
    )?))
}

/// Validate a raw `SYNTHEOS_STRIPE_WEBHOOK_SECRET` value.
///
/// `None` means the variable was unset: the Stripe webhook is not mounted and the kernel server
/// behaves exactly as before.
///
/// A set-but-blank secret is an `Err`. An empty HMAC key is still a *valid* HMAC key (HMAC
/// accepts any key length), so a blank secret would not fail loudly -- it would quietly stand up
/// a public webhook that anyone able to compute `HMAC("", payload)` could forge. That is a hard
/// boot error, matching the operator surface's posture: a misconfigured secret must never
/// degrade into a quietly-insecure one.
///
/// A valid secret is returned verbatim. It is deliberately never trimmed: the trimmed and
/// untrimmed strings are different HMAC keys, so silently trimming would produce a server that
/// rejects every genuine Stripe delivery for a reason no log would explain.
///
/// Split out from [`billing_state_from_env`] so this policy is unit-testable without a live
/// Postgres connection to build a `PlutusStore` from.
fn validated_webhook_secret(raw: Option<String>) -> Result<Option<String>, String> {
    match raw {
        None => Ok(None),
        Some(secret) if secret.trim().is_empty() => Err(
            "SYNTHEOS_STRIPE_WEBHOOK_SECRET is set but empty: unset it to disable the billing \
             webhook, or set it to the Stripe endpoint signing secret"
                .to_string(),
        ),
        Some(secret) => Ok(Some(secret)),
    }
}

/// Build a [`BillingState`] from the environment when `SYNTHEOS_STRIPE_WEBHOOK_SECRET` is set.
///
/// Returns `Ok(None)` when the variable is absent. Returns `Err` when it is present but blank
/// (see [`validated_webhook_secret`]), or when it is present but not valid Unicode -- the latter
/// is a hard error rather than a silent "unset", so a mangled secret can never leave the webhook
/// quietly unmounted when an operator intended it enabled.
fn billing_state_from_env(
    plutus_store: Option<Arc<PlutusStore>>,
) -> Result<Option<BillingState>, Box<dyn std::error::Error>> {
    let raw = match std::env::var("SYNTHEOS_STRIPE_WEBHOOK_SECRET") {
        Ok(secret) => Some(secret),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => return Err(format!("SYNTHEOS_STRIPE_WEBHOOK_SECRET: {e}").into()),
    };
    match validated_webhook_secret(raw)? {
        None => Ok(None),
        Some(secret) => {
            let store = plutus_store.ok_or(
                "Stripe billing requires the PostgreSQL policy backend; disable local policy mode",
            )?;
            Ok(Some(BillingState::new(store, secret)))
        }
    }
}

/// Build an [`OperatorState`] from the environment when
/// `SYNTHEOS_OPERATOR_JWT_SECRET` is set.
///
/// Returns `Ok(None)` when the variable is unset -- the operator surface is
/// disabled and the kernel server behaves exactly as before.
///
/// Returns `Err` when the variable IS set but is malformed or decodes to fewer
/// than 32 bytes. A misconfigured secret is always a hard boot error so the
/// operator surface is never silently absent when an operator intended it enabled.
///
/// Secret encoding: if the value is a valid even-length lowercase or uppercase
/// hex string it is decoded as hex (64 chars -> 32 bytes). Otherwise the raw
/// UTF-8 bytes are used as-is. Both paths require >= 32 bytes.
#[allow(clippy::too_many_arguments)]
async fn operator_state_from_env(
    directory_store: Arc<SqliteDirectory>,
    plutus_store: Arc<dyn PolicyBackend>,
    soma: Arc<SomaStore>,
    chiasm: Arc<ChiasmStore>,
    broca: Arc<BrocaStore>,
    thymus: Arc<ThymusStore>,
    loom: Arc<LoomStore>,
    bus: Arc<AxonBus>,
) -> Result<Option<OperatorState>, Box<dyn std::error::Error>> {
    // Unset -> operator surface disabled; kernel server unchanged.
    let raw = match std::env::var("SYNTHEOS_OPERATOR_JWT_SECRET") {
        Ok(v) if !v.is_empty() => v,
        _ => return Ok(None),
    };

    // Attempt hex decoding first; fall back to raw UTF-8 bytes.
    let secret_bytes: Vec<u8> = if raw.len() % 2 == 0 && raw.chars().all(|c| c.is_ascii_hexdigit())
    {
        hex::decode(raw.trim())
            .map_err(|e| format!("SYNTHEOS_OPERATOR_JWT_SECRET hex decode failed: {e}"))?
    } else {
        raw.into_bytes()
    };

    // A set-but-too-short secret is a hard boot error -- never a silent fallback.
    if secret_bytes.len() < 32 {
        return Err(
            "SYNTHEOS_OPERATOR_JWT_SECRET must be >= 32 bytes (64 hex chars or 32+ ASCII chars)"
                .into(),
        );
    }

    // Resolve the browser/webview CORS allow-list (SYNTHEOS_OPERATOR_CORS_ORIGINS, defaults to
    // the Tauri webview origins). Validated here, alongside the JWT secret above, so a malformed
    // entry is a hard boot error rather than a silent "every browser client is blocked" surprise
    // discovered later at request time.
    let cors_origins = syntheos_server::operator::cors_origins_from_env()
        .map_err(|e| format!("SYNTHEOS_OPERATOR_CORS_ORIGINS: {e}"))?;

    // Build OperatorState sharing the same Arcs the kernel uses.
    let op_state = OperatorState {
        accounts: directory_store,
        plutus: plutus_store,
        jwt_secret: Arc::new(secret_bytes),
        soma,
        chiasm,
        broca,
        thymus,
        loom,
        axon: bus,
        cors_origins: Arc::new(cors_origins),
    };

    Ok(Some(op_state))
}

/// Return whether both first-operator bootstrap values are configured.
fn operator_bootstrap_requested() -> Result<bool, String> {
    let email = optional_env("SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL")?;
    let mut password = optional_env("SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD")?;
    let result = bootstrap_pair_requested(email.as_deref(), password.as_deref());
    if let Some(password) = password.as_mut() {
        password.zeroize();
    }
    result
}

/// Validate that operator bootstrap credentials are either both present or both absent.
fn bootstrap_pair_requested(email: Option<&str>, password: Option<&str>) -> Result<bool, String> {
    let email_present = email.is_some_and(|value| !value.trim().is_empty());
    let password_present = password.is_some_and(|value| !value.is_empty());
    match (email_present, password_present) {
        (false, false) => Ok(false),
        (true, true) => Ok(true),
        _ => Err(
            "SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL and SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD must be set together"
                .to_string(),
        ),
    }
}

/// Bootstrap the first operator account when the bootstrap environment variables
/// are present and the account does not yet exist.
///
/// Reads:
/// - `SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL` -- the email address to create.
/// - `SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD` -- the plaintext password to hash.
///
/// When both are set AND no account for that email exists yet:
/// 1. Reuse an existing account principal or create the account with an Argon2id hash.
/// 2. Reuse its Plutus tenant or create one with the principal as `Owner`.
///
/// The ordering permits a retry to finish tenant creation after a transient PostgreSQL failure.
async fn bootstrap_operator_if_configured(
    directory: &SqliteDirectory,
    plutus_store: &PlutusStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let email = match std::env::var("SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL") {
        Ok(e) if !e.is_empty() => e,
        _ => return Ok(()), // bootstrap env var not set; nothing to do
    };
    let password = Zeroizing::new(
        match std::env::var("SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD") {
            Ok(p) if !p.is_empty() => p,
            _ => return Ok(()), // bootstrap env var not set; nothing to do
        },
    );

    // Reuse an existing account so a prior Postgres failure can finish bootstrap on retry.
    let principal = if let Some(account) = directory
        .get_account(&email)
        .map_err(|e| format!("bootstrap get_account: {e}"))?
    {
        account.principal
    } else {
        let principal = directory
            .enroll(PrincipalKind::Human, Some(format!("operator:{email}")))
            .await
            .map_err(|e| format!("bootstrap enroll: {e}"))?;
        let account_result = directory
            .create_account(&email, &password, principal.id)
            .map_err(|e| format!("bootstrap create_account: {e}"));
        account_result?;
        principal.id
    };

    if plutus_store
        .tenant_for_principal(principal)
        .await
        .map_err(|e| format!("bootstrap tenant lookup: {e}"))?
        .is_some()
    {
        tracing::debug!("operator account and tenant already exist; skipping bootstrap");
        return Ok(());
    }

    let tenant = TenantId::new();
    plutus_store
        .create_org(tenant, "operator", principal, QuotaTier::Enterprise)
        .await
        .map_err(|e| format!("bootstrap create_org: {e}"))?;

    tracing::info!(
        principal_id = %principal,
        tenant_id    = %tenant,
        "operator bootstrap complete: account created (Owner in new Enterprise org)"
    );

    Ok(())
}

/// Resolve a service database path from `var`, falling back to `default`.
fn db_path(var: &str, default: &str) -> Result<String, std::io::Error> {
    Ok(std::env::var(var).unwrap_or_else(|_| default.to_string()))
}

/// Read and validate the Plutus pool acquisition deadline from the process environment.
fn plutus_acquire_timeout_from_env() -> Result<Duration, String> {
    let raw = match std::env::var("SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(format!("SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS: {error}")),
    };
    validated_plutus_acquire_timeout(raw.as_deref())
}

/// Read and validate the production Loom timeout sweep interval from the environment.
fn loom_timeout_sweep_interval_from_env() -> Result<Duration, String> {
    let raw = match std::env::var("SYNTHEOS_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => {
            return Err(format!(
                "SYNTHEOS_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS: {error}"
            ));
        }
    };
    validated_loom_timeout_sweep_interval(raw.as_deref())
}

/// Validate an optional Loom timeout sweep interval and use the production default when absent.
fn validated_loom_timeout_sweep_interval(raw: Option<&str>) -> Result<Duration, String> {
    let seconds = match raw {
        Some(value) => value.parse::<u64>().map_err(|_| {
            "SYNTHEOS_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS must be an integer from 1 through 300"
                .to_string()
        })?,
        None => DEFAULT_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS,
    };
    if !(1..=MAX_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS).contains(&seconds) {
        return Err(
            "SYNTHEOS_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS must be an integer from 1 through 300"
                .to_string(),
        );
    }
    Ok(Duration::from_secs(seconds))
}

/// Enforce Loom step deadlines periodically until coordinated server shutdown.
async fn run_loom_timeout_sweeper(
    loom: Arc<LoomStore>,
    sweep_interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(sweep_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                match loom.sweep_timeouts().await {
                    Ok(timed_out) if !timed_out.is_empty() => {
                        tracing::warn!(
                            timed_out_steps = timed_out.len(),
                            "Loom timeout sweep enforced overdue step deadlines"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::error!(%error, "Loom timeout sweep failed");
                    }
                }
            }
        }
    }
}

/// Validate an optional Plutus acquisition timeout and return the production default when absent.
fn validated_plutus_acquire_timeout(raw: Option<&str>) -> Result<Duration, String> {
    let seconds = match raw {
        Some(value) => value.parse::<u64>().map_err(|_| {
            "SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS must be an integer from 1 through 300".to_string()
        })?,
        None => DEFAULT_PLUTUS_ACQUIRE_TIMEOUT_SECS,
    };
    if !(1..=MAX_PLUTUS_ACQUIRE_TIMEOUT_SECS).contains(&seconds) {
        return Err(
            "SYNTHEOS_PLUTUS_ACQUIRE_TIMEOUT_SECS must be an integer from 1 through 300"
                .to_string(),
        );
    }
    Ok(Duration::from_secs(seconds))
}

/// The fallback logged in place of a connection string this function cannot confidently parse.
/// Never the raw input -- an unrecognized shape might still carry a password we failed to find.
const REDACTED_URL_PLACEHOLDER: &str = "<redacted: unparseable connection string>";

/// Mask the password in a Postgres connection URL's userinfo before it is safe to log.
///
/// `postgres://user:pw@host/db` becomes `postgres://user:***@host/db` -- the host and database
/// stay visible (useful for diagnosing which instance a log line refers to) while the password
/// never appears in the output. A URL with no userinfo, or userinfo with no password, is
/// returned unchanged (there is nothing to redact). A string this function cannot confidently
/// recognize as `scheme://...` falls back to [`REDACTED_URL_PLACEHOLDER`] rather than risk
/// leaking a password hiding in a shape it did not anticipate.
fn redact_postgres_password(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return REDACTED_URL_PLACEHOLDER.to_string();
    };
    let scheme = &url[..scheme_end + 3];
    let rest = &url[scheme_end + 3..];

    // The authority (userinfo@host:port) ends at the first '/' or '?' after the scheme; the
    // rest (path/query) is passed through untouched.
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let tail = &rest[authority_end..];

    // The last '@' in the authority separates userinfo from host (a password could itself
    // contain '@', though that is unusual for an unencoded connection string).
    let Some(at_idx) = authority.rfind('@') else {
        return url.to_string(); // no userinfo at all -- nothing to redact
    };
    let userinfo = &authority[..at_idx];
    let host_part = &authority[at_idx..]; // includes the leading '@'

    match userinfo.find(':') {
        Some(colon_idx) => {
            let user = &userinfo[..colon_idx];
            format!("{scheme}{user}:***{host_part}{tail}")
        }
        // Userinfo present but no ':pw' -- e.g. `postgres://user@host/db`. Nothing to redact.
        None => url.to_string(),
    }
}

#[cfg(test)]
/// Unit tests for the `SYNTHEOS_STRIPE_WEBHOOK_SECRET` boot policy.
mod billing_env_tests {
    use super::*;

    /// An absent variable disables the webhook rather than failing the boot.
    #[test]
    fn absent_secret_disables_the_webhook() {
        assert_eq!(validated_webhook_secret(None).expect("absent is ok"), None);
    }

    /// An empty secret is a hard boot error: an empty HMAC key still verifies signatures, so
    /// accepting it would stand up a forgeable public webhook.
    #[test]
    fn empty_secret_is_a_hard_boot_error() {
        assert!(validated_webhook_secret(Some(String::new())).is_err());
    }

    /// A whitespace-only secret is equally a boot error, not a usable key.
    #[test]
    fn whitespace_only_secret_is_a_hard_boot_error() {
        assert!(validated_webhook_secret(Some("   \t\n".to_string())).is_err());
    }

    /// A real secret is passed through byte-for-byte. Trimming it would change the HMAC key
    /// and silently reject every genuine Stripe delivery.
    #[test]
    fn valid_secret_is_never_trimmed() {
        let padded = " whsec_abc ".to_string();
        assert_eq!(
            validated_webhook_secret(Some(padded.clone())).expect("valid"),
            Some(padded)
        );
    }
}

#[cfg(test)]
/// Unit tests for [`redact_postgres_password`].
mod redact_tests {
    use super::*;

    /// A standard `user:pw@host/db` URL has its password replaced with `***`; host and
    /// database stay visible for diagnostics.
    #[test]
    fn redact_postgres_password_masks_password() {
        let redacted =
            redact_postgres_password("postgres://plutus:hunter2@db.internal:5432/plutus");
        assert_eq!(redacted, "postgres://plutus:***@db.internal:5432/plutus");
    }

    /// A URL with a user but no password is returned unchanged -- there is nothing to redact.
    #[test]
    fn redact_postgres_password_leaves_url_without_password_unchanged() {
        let url = "postgres://plutus@db.internal:5432/plutus";
        assert_eq!(redact_postgres_password(url), url);
    }

    /// A URL with no userinfo at all (no `@`) is returned unchanged.
    #[test]
    fn redact_postgres_password_leaves_url_without_userinfo_unchanged() {
        let url = "postgres://db.internal:5432/plutus";
        assert_eq!(redact_postgres_password(url), url);
    }

    /// A string with no recognizable `scheme://` falls back to the generic placeholder --
    /// never the raw (possibly password-bearing) input.
    #[test]
    fn redact_postgres_password_falls_back_to_placeholder_for_unparseable_input() {
        let redacted = redact_postgres_password("not a connection string at all");
        assert_eq!(redacted, REDACTED_URL_PLACEHOLDER);
        assert!(!redacted.contains("not a connection string"));
    }
}

#[cfg(test)]
/// Unit tests for the Plutus pool acquisition deadline policy.
mod plutus_timeout_tests {
    use super::*;

    /// An absent override uses the bounded production default.
    #[test]
    fn absent_timeout_uses_default() {
        assert_eq!(
            validated_plutus_acquire_timeout(None).expect("default must be valid"),
            Duration::from_secs(DEFAULT_PLUTUS_ACQUIRE_TIMEOUT_SECS)
        );
    }

    /// The smallest supported timeout is accepted for fast-failing probes and deployments.
    #[test]
    fn one_second_timeout_is_accepted() {
        assert_eq!(
            validated_plutus_acquire_timeout(Some("1")).expect("one second must be valid"),
            Duration::from_secs(1)
        );
    }

    /// Zero cannot silently disable the deadline or force every acquisition to fail immediately.
    #[test]
    fn zero_timeout_is_rejected() {
        assert!(validated_plutus_acquire_timeout(Some("0")).is_err());
    }

    /// Malformed values are configuration errors rather than implicit defaults.
    #[test]
    fn malformed_timeout_is_rejected() {
        assert!(validated_plutus_acquire_timeout(Some("soon")).is_err());
    }

    /// Values above the operational ceiling cannot recreate an effectively unbounded boot stall.
    #[test]
    fn excessive_timeout_is_rejected() {
        assert!(validated_plutus_acquire_timeout(Some("301")).is_err());
    }
}

#[cfg(test)]
/// Unit and lifecycle tests for the production Loom timeout sweeper.
mod loom_timeout_sweeper_tests {
    use super::*;
    use henosis_loom::{NewWorkflow, StepDef, StepStatus, StepType};
    use syntheos_contracts::{PrincipalId, TenantId};

    /// An absent override uses the bounded production default.
    #[test]
    fn absent_interval_uses_default() {
        assert_eq!(
            validated_loom_timeout_sweep_interval(None).expect("default must be valid"),
            Duration::from_secs(DEFAULT_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS)
        );
    }

    /// The supported interval boundaries are accepted.
    #[test]
    fn interval_boundaries_are_accepted() {
        assert_eq!(
            validated_loom_timeout_sweep_interval(Some("1")).expect("minimum must be valid"),
            Duration::from_secs(1)
        );
        assert_eq!(
            validated_loom_timeout_sweep_interval(Some("300")).expect("maximum must be valid"),
            Duration::from_secs(MAX_LOOM_TIMEOUT_SWEEP_INTERVAL_SECS)
        );
    }

    /// Zero, malformed, and excessive intervals fail closed at startup.
    #[test]
    fn invalid_intervals_are_rejected() {
        assert!(validated_loom_timeout_sweep_interval(Some("0")).is_err());
        assert!(validated_loom_timeout_sweep_interval(Some("soon")).is_err());
        assert!(validated_loom_timeout_sweep_interval(Some("301")).is_err());
    }

    /// The production loop fails an overdue step and exits promptly when shutdown is requested.
    #[tokio::test]
    async fn sweeper_enforces_deadlines_and_obeys_shutdown() {
        let bus = Arc::new(AxonBus::new());
        let loom = Arc::new(LoomStore::open_in_memory(bus).expect("open Loom"));
        let principal = PrincipalId::new();
        let workflow = loom
            .create_workflow(NewWorkflow {
                tenant: TenantId::new(),
                principal_id: principal,
                name: "timeout-sweeper-test".to_string(),
                description: None,
                steps: vec![StepDef {
                    name: "overdue".to_string(),
                    step_type: StepType::Action,
                    config: None,
                    depends_on: None,
                    max_retries: Some(0),
                    timeout_ms: Some(0),
                }],
            })
            .await
            .expect("create workflow");
        let run = loom
            .create_run(principal, workflow.id, None)
            .await
            .expect("create run");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let sweeper = tokio::spawn(run_loom_timeout_sweeper(
            loom.clone(),
            Duration::from_millis(1),
            shutdown_rx,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let steps = loom.get_steps(principal, run.id).await.expect("read steps");
                if steps.iter().any(|step| step.status == StepStatus::Failed) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("sweeper must enforce the deadline");

        shutdown_tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), sweeper)
            .await
            .expect("sweeper must stop promptly")
            .expect("sweeper task must not panic");
    }
}

#[cfg(test)]
/// Unit tests for the integrated server's fail-closed bind policy.
mod bind_policy_tests {
    use super::*;

    /// IPv4 loopback is accepted.
    #[test]
    fn ipv4_loopback_is_accepted() {
        let addr = validated_bind_addr("127.0.0.1:8088").expect("loopback must be valid");
        assert!(addr.ip().is_loopback());
    }

    /// IPv6 loopback is accepted.
    #[test]
    fn ipv6_loopback_is_accepted() {
        let addr = validated_bind_addr("[::1]:8088").expect("loopback must be valid");
        assert!(addr.ip().is_loopback());
    }

    /// A wildcard bind is accepted because public routes authenticate their callers.
    #[test]
    fn wildcard_bind_is_accepted() {
        let addr = validated_bind_addr("0.0.0.0:8088").expect("wildcard bind must be valid");
        assert!(addr.ip().is_unspecified());
    }

    /// A concrete non-loopback bind is accepted for authenticated deployments.
    #[test]
    fn remote_bind_is_accepted() {
        let addr = validated_bind_addr("192.0.2.1:8088").expect("remote bind must be accepted");
        assert_eq!(addr.to_string(), "192.0.2.1:8088");
    }

    /// Hostnames and malformed values fail with a stable configuration error.
    #[test]
    fn malformed_address_is_rejected() {
        let error = validated_bind_addr("localhost:8088").expect_err("hostname must fail");
        assert!(error.contains("must be an IP socket address"));
    }
}

#[cfg(test)]
/// Unit tests for broker endpoint loopback classification.
mod phylaxd_url_tests {
    use super::*;

    /// URL host serialization encloses IPv6 literals in brackets, which remain loopback.
    #[test]
    fn bracketed_ipv6_loopback_is_accepted() {
        let url = reqwest::Url::parse("http://[::1]:8089").expect("URL must parse");
        assert!(is_loopback_url_host(
            url.host_str().expect("host must exist")
        ));
    }

    /// IPv4 loopback and localhost remain accepted.
    #[test]
    fn conventional_loopback_hosts_are_accepted() {
        assert!(is_loopback_url_host("127.0.0.1"));
        assert!(is_loopback_url_host("localhost"));
        assert!(is_loopback_url_host("LOCALHOST"));
    }

    /// Non-loopback and malformed host strings never gain loopback privileges.
    #[test]
    fn remote_and_malformed_hosts_are_rejected() {
        assert!(!is_loopback_url_host("[::]"));
        assert!(!is_loopback_url_host("192.0.2.1"));
        assert!(!is_loopback_url_host("[::1"));
        assert!(!is_loopback_url_host("::1]"));
    }
}

#[cfg(test)]
/// Unit tests for strict first-operator bootstrap configuration.
mod operator_bootstrap_tests {
    use super::*;

    /// Both absent values leave browser operator bootstrap disabled.
    #[test]
    fn absent_pair_is_disabled() {
        assert!(!bootstrap_pair_requested(None, None).expect("absent pair must be valid"));
    }

    /// Both present values enable browser operator bootstrap.
    #[test]
    fn complete_pair_is_enabled() {
        assert!(
            bootstrap_pair_requested(Some("owner@example.test"), Some("secret"))
                .expect("complete pair must be valid")
        );
    }

    /// A partial pair is rejected instead of silently skipping bootstrap.
    #[test]
    fn partial_pair_is_rejected() {
        assert!(bootstrap_pair_requested(Some("owner@example.test"), None).is_err());
        assert!(bootstrap_pair_requested(None, Some("secret")).is_err());
    }
}

#[cfg(test)]
/// Unit tests for the explicit no-prompt local auto-initialization switch.
mod auto_init_tests {
    use super::*;

    /// Distinguishes local and live commands before the runtime starts.
    #[test]
    fn control_commands_are_classified_before_execution() {
        assert!(control_command(&Command::Status));
        assert!(control_command(&Command::Token(
            syntheos_server::cli::TokenCommand::List
        )));
        assert!(!control_command(&Command::Init(InitMode::Quick)));
        assert!(!control_command(&Command::Serve));
    }

    /// An absent switch leaves filesystem initialization under explicit CLI control.
    #[test]
    fn absent_switch_is_disabled() {
        assert!(!auto_init_requested(None).expect("absent switch must be valid"));
    }

    /// The exact quick token enables the idempotent local initializer.
    #[test]
    fn exact_quick_switch_is_enabled() {
        assert!(auto_init_requested(Some("quick")).expect("quick switch must be valid"));
    }

    /// Unknown values fail instead of silently initializing an unintended environment.
    #[test]
    fn unknown_switch_is_rejected() {
        assert!(auto_init_requested(Some("1")).is_err());
    }
}

#[cfg(test)]
/// Unit tests for bounded, no-follow supervisor rule loading.
mod supervisor_rules_file_tests {
    use super::*;

    /// Create a unique directory and return it with a child rules path.
    fn temporary_rules_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let directory = std::env::temp_dir().join(format!(
            "henosis-supervisor-rules-{}",
            syntheos_contracts::EventId::new()
        ));
        std::fs::create_dir(&directory).expect("create rules directory");
        let path = directory.join("rules.json");
        (directory, path)
    }

    /// A regular, bounded rules document loads successfully.
    #[test]
    fn regular_rules_file_loads() {
        let (directory, path) = temporary_rules_path();
        let encoded =
            serde_json::to_vec(&supervisor::default_rules()).expect("encode default rules");
        std::fs::write(&path, encoded).expect("write rules");

        let loaded = load_supervisor_rules(&path).expect("load regular rules");
        assert_eq!(loaded.len(), supervisor::default_rules().len());

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A rules document larger than the fixed startup budget is rejected.
    #[test]
    fn oversized_rules_file_is_rejected() {
        let (directory, path) = temporary_rules_path();
        std::fs::write(
            &path,
            vec![b' '; MAX_SUPERVISOR_RULES_FILE_BYTES as usize + 1],
        )
        .expect("write oversized rules");

        let error = load_supervisor_rules(&path).expect_err("oversized rules must fail");
        assert!(error.to_string().contains("maximum size"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Invalid JSON remains a startup error after the descriptor hardening.
    #[test]
    fn malformed_rules_file_is_rejected() {
        let (directory, path) = temporary_rules_path();
        std::fs::write(&path, b"{not-json").expect("write malformed rules");

        load_supervisor_rules(&path).expect_err("malformed rules must fail");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A non-regular rules path is rejected before parsing.
    #[test]
    fn non_regular_rules_path_is_rejected() {
        let (directory, path) = temporary_rules_path();
        std::fs::create_dir(&path).expect("create directory at rules path");

        load_supervisor_rules(&path).expect_err("directory rules path must fail");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A symbolic-link rules leaf is never followed.
    #[cfg(unix)]
    #[test]
    fn symbolic_link_rules_path_is_rejected() {
        use std::os::unix::fs::symlink;

        let (directory, path) = temporary_rules_path();
        let target = directory.join("target.json");
        let encoded =
            serde_json::to_vec(&supervisor::default_rules()).expect("encode default rules");
        std::fs::write(&target, encoded).expect("write target rules");
        symlink(&target, &path).expect("create rules symlink");

        load_supervisor_rules(&path).expect_err("symbolic-link rules path must fail");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A Windows file reparse point is rejected without parsing its target.
    #[cfg(windows)]
    #[test]
    fn windows_symbolic_link_rules_path_is_rejected() {
        use std::os::windows::fs::symlink_file;

        let (directory, path) = temporary_rules_path();
        let target = directory.join("target.json");
        let encoded =
            serde_json::to_vec(&supervisor::default_rules()).expect("encode default rules");
        std::fs::write(&target, encoded).expect("write target rules");
        symlink_file(&target, &path).expect("create rules symlink");

        load_supervisor_rules(&path).expect_err("Windows symbolic-link rules path must fail");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A Unix FIFO is rejected through a nonblocking open.
    #[cfg(unix)]
    #[test]
    fn fifo_rules_path_is_rejected_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let (directory, path) = temporary_rules_path();
        let encoded = CString::new(path.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `encoded` is a live NUL-terminated path and the mode is valid.
        let result = unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) };
        assert_eq!(
            result,
            0,
            "create FIFO: {}",
            std::io::Error::last_os_error()
        );

        load_supervisor_rules(&path).expect_err("FIFO rules path must fail");

        let _ = std::fs::remove_dir_all(&directory);
    }
}

#[cfg(test)]
/// Unit tests for the explicit local Plutus policy boundary.
mod local_policy_tests {
    use super::*;
    use henosis_pistis::PistisGate;
    use syntheos_contracts::{Gate, GateDecision, GateRequest, RequestContext, ToolInvocation};

    /// Build valid local identity strings for pure configuration tests.
    fn local_ids() -> (String, String) {
        (
            TenantId::new().to_string(),
            syntheos_contracts::PrincipalId::new().to_string(),
        )
    }

    /// An absent opt-in leaves the production PostgreSQL selection untouched.
    #[test]
    fn absent_local_flag_selects_no_local_backend() {
        let result = validated_local_policy_config(
            None,
            Some("postgres://db/henosis"),
            "127.0.0.1:8088".parse().unwrap(),
            None,
            None,
        )
        .unwrap();
        assert!(result.is_none());
    }

    /// A valid identity pair enables local policy on loopback without PostgreSQL.
    #[test]
    fn exact_local_flag_accepts_loopback_identity() {
        let (tenant, principal) = local_ids();
        let config = validated_local_policy_config(
            Some("1"),
            None,
            "127.0.0.1:8088".parse().unwrap(),
            Some(&tenant),
            Some(&principal),
        )
        .unwrap()
        .expect("local config");
        assert_eq!(config.tenant.to_string(), tenant);
        assert_eq!(config.principal.to_string(), principal);
    }

    /// Local policy and PostgreSQL cannot both claim the Plutus slot.
    #[test]
    fn local_and_postgres_are_mutually_exclusive() {
        let (tenant, principal) = local_ids();
        let error = validated_local_policy_config(
            Some("1"),
            Some("postgres://db/henosis"),
            "127.0.0.1:8088".parse().unwrap(),
            Some(&tenant),
            Some(&principal),
        )
        .expect_err("dual backend selection must fail");
        assert!(error.contains("mutually exclusive"));
    }

    /// The general remote-development override cannot expose local policy mode.
    #[test]
    fn local_policy_rejects_non_loopback_bind() {
        let (tenant, principal) = local_ids();
        let error = validated_local_policy_config(
            Some("1"),
            None,
            "192.0.2.1:8088".parse().unwrap(),
            Some(&tenant),
            Some(&principal),
        )
        .expect_err("local policy must remain loopback-only");
        assert!(error.contains("loopback"));
    }

    /// Local policy rejects missing and malformed identity values.
    #[test]
    fn local_policy_requires_valid_identity() {
        let (_, principal) = local_ids();
        assert!(validated_local_policy_config(
            Some("1"),
            None,
            "127.0.0.1:8088".parse().unwrap(),
            None,
            Some(&principal),
        )
        .is_err());
        assert!(validated_local_policy_config(
            Some("1"),
            None,
            "127.0.0.1:8088".parse().unwrap(),
            Some("invalid"),
            Some(&principal),
        )
        .is_err());
    }

    /// Validated local policy produces one verified admission with only the probe capability.
    #[tokio::test]
    async fn local_policy_builds_probe_only_pistis_room() {
        let (tenant, principal) = local_ids();
        let config = validated_local_policy_config(
            Some("1"),
            None,
            "127.0.0.1:8088".parse().unwrap(),
            Some(&tenant),
            Some(&principal),
        )
        .unwrap()
        .expect("validated local policy");
        let (source, trust) =
            pistis_authority_from_local_policy(Some(&config)).expect("build local Pistis room");
        let scope = RoomScope::new(config.tenant, LOCAL_PISTIS_ROOM_ID);
        let state = source.room_state(&scope).expect("local room state");
        let verified = state
            .verify_for(&scope, trust.as_ref())
            .expect("verify signed local room");
        let admission = verified
            .trusted_admission(&config.principal)
            .expect("configured principal admission");

        assert_eq!(admission.admitted_capabilities.len(), 1);
        assert_eq!(admission.admitted_capabilities[0].name, "henosis");
        assert_eq!(
            admission.admitted_capabilities[0].action_kinds,
            BTreeSet::from([ActionKind::Message])
        );
        assert!(admission.admitted_capabilities[0].expires_at.is_none());

        let gate = PistisGate::new(source, trust);
        let probe = GateRequest {
            context: RequestContext {
                tenant: config.tenant,
                principal: config.principal,
                persona: None,
                session: None,
                room: Some(LOCAL_PISTIS_ROOM_ID.to_string()),
                task: None,
                workflow: None,
                authority: None,
            },
            invocation: ToolInvocation {
                tool: "henosis".to_string(),
                action: "probe".to_string(),
                args: serde_json::json!({}),
            },
        };
        assert_eq!(gate.check(&probe).await.unwrap(), GateDecision::Allow);

        let mut wrong_room = probe.clone();
        wrong_room.context.room = Some("!other-local:loopback".to_string());
        assert!(matches!(
            gate.check(&wrong_room).await.unwrap(),
            GateDecision::Deny { .. }
        ));

        let mut wrong_tenant = probe.clone();
        wrong_tenant.context.tenant = TenantId::new();
        assert!(matches!(
            gate.check(&wrong_tenant).await.unwrap(),
            GateDecision::Deny { .. }
        ));

        let mut wrong_principal = probe.clone();
        wrong_principal.context.principal = syntheos_contracts::PrincipalId::new();
        assert!(matches!(
            gate.check(&wrong_principal).await.unwrap(),
            GateDecision::Deny { .. }
        ));

        let mut unknown_action = probe.clone();
        unknown_action.invocation.action = "unknown".to_string();
        assert!(matches!(
            gate.check(&unknown_action).await.unwrap(),
            GateDecision::Deny { .. }
        ));

        let mut unrelated = probe;
        unrelated.invocation = ToolInvocation {
            tool: "gmail".to_string(),
            action: "read".to_string(),
            args: serde_json::json!({}),
        };
        assert!(matches!(
            gate.check(&unrelated).await.unwrap(),
            GateDecision::Deny { .. }
        ));
    }

    /// Absent local policy leaves both production Pistis authority inputs empty.
    #[test]
    fn production_builds_empty_fail_closed_pistis_authority() {
        let (tenant, principal) = local_ids();
        let local_config = LocalPolicyConfig {
            tenant: tenant.parse().unwrap(),
            principal: principal.parse().unwrap(),
        };
        let (local_source, _) = pistis_authority_from_local_policy(Some(&local_config))
            .expect("build local comparison room");
        let scope = RoomScope::new(local_config.tenant, LOCAL_PISTIS_ROOM_ID);
        let state = local_source
            .room_state(&scope)
            .expect("local comparison state");
        let (production_source, production_trust) =
            pistis_authority_from_local_policy(None).expect("build production Pistis authority");

        assert!(production_source.room_state(&scope).is_none());
        assert!(state.verify_for(&scope, production_trust.as_ref()).is_err());
    }
}

/// Resolve when a shutdown signal is received, so `axum` can drain in-flight requests.
///
/// Listens for both SIGINT (Ctrl-C) and, on Unix, SIGTERM -- the latter is what systemd sends on
/// `stop`/`restart`, so service management drains cleanly instead of being killed.
async fn shutdown_signal() {
    /// Wait for SIGINT (Ctrl-C); an install failure is logged and the arm never resolves.
    async fn sigint() {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %err, "failed to install Ctrl-C handler");
            std::future::pending::<()>().await;
        }
    }

    /// Wait for SIGTERM on Unix; an install failure is logged and the arm never resolves.
    #[cfg(unix)]
    async fn sigterm() {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    }

    /// Non-Unix platforms have no SIGTERM; this arm never resolves.
    #[cfg(not(unix))]
    async fn sigterm() {
        std::future::pending::<()>().await;
    }

    tokio::select! {
        _ = sigint() => {},
        _ = sigterm() => {},
    }
    tracing::info!("shutdown signal received, draining");
}
