//! `syntheos-server` binary: the single entry point that boots the Henosis foundation and serves
//! the Phase 0 HTTP surface.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use henosis_broca::BrocaStore;
use henosis_chiasm::ChiasmStore;
use henosis_eidolon::supervisor::{self, Supervisor, SupervisorConfig};
use henosis_eidolon::{EidolonOutputFilter, EidolonPolicy};
use henosis_loom::{CompositeStepExecutor, HephaestusDispatch, HephaestusStepExecutor, LoomStore, TransformExecutor};
use henosis_phylax::PhylaxStore;
use henosis_pistis::{InMemoryRoomStateSource, RoomStateSource};
use henosis_plutus::{PlutusStore, PolicyBackend, QuotaTier};
use henosis_rift::{Approver, RegistryApprover};
use henosis_soma::SomaStore;
use henosis_thymus::ThymusStore;
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalKind, TenantId};
use syntheos_dispatch::deny::DenyExecutor;
use syntheos_dispatch::Dispatcher;
use syntheos_identity::{PrincipalDirectory, SqliteDirectory};
use syntheos_server::billing::BillingState;
use syntheos_server::operator::OperatorState;
use syntheos_server::{live_gate_chain, SomaQualitySink};
use syntheos_server::{router, AppState};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tracing_subscriber::EnvFilter;

/// Largest request body the server accepts, in bytes (1 MiB). Phase 0 payloads are small JSON;
/// anything bigger is rejected before it can exhaust memory.
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Wire the foundation: bus and directory first, stores next, then the dispatcher (its
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

    // Phase 1 kernel services: persistent SQLite at configurable paths (migrations apply on
    // open).
    let chiasm_db = db_path("SYNTHEOS_CHIASM_DB", "data/chiasm.sqlite")?;
    let chiasm = Arc::new(ChiasmStore::open(&chiasm_db, bus.clone())?);
    tracing::info!(path = %chiasm_db, "chiasm task store open");
    let soma_db = db_path("SYNTHEOS_SOMA_DB", "data/soma.sqlite")?;
    let soma = Arc::new(SomaStore::open(&soma_db, bus.clone(), directory.clone())?);
    tracing::info!(path = %soma_db, "soma presence store open");
    // No LLM narrator is attached in Phase 1 (template-or-nothing); a Synapse/Foundry-backed
    // Narrator plugs in via BrocaStore::with_narrator when Phase 4 lands.
    let broca_db = db_path("SYNTHEOS_BROCA_DB", "data/broca.sqlite")?;
    let broca = Arc::new(BrocaStore::open(&broca_db, bus.clone())?);
    tracing::info!(path = %broca_db, "broca narration log open");
    // Build the in-process Hephaestus executor from env (Config::from_env). If provider
    // credentials are absent the AppState still constructs and attaches; individual task
    // executions will fail with a meaningful auth error rather than silently succeeding.
    let heph_state =
        henosis_hephaestus::build_state(henosis_hephaestus::Config::from_env());
    let heph_dispatch = HephaestusRuntimeDispatch { state: heph_state };

    // CompositeStepExecutor: Transform handles pure-JSON steps inline; Hephaestus dispatches
    // agent tasks to the in-process executor (story 5.5). First-match wins; unclaimed types
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

    // The Plutus policy authority (Story 6.x / row 1): a real gate replacing the last deny-stub.
    // Org/role/quota state persists in Postgres at SYNTHEOS_PLUTUS_DB (required). On first boot
    // the operator principal is bootstrapped into a default org so the dispatch path is usable;
    // subsequent orgs/members are managed through Plutus APIs.
    // SYNTHEOS_PLUTUS_OPERATOR_TENANT + SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL are required for
    // the bootstrap. If the org already exists, bootstrap is a no-op.
    let plutus_url = std::env::var("SYNTHEOS_PLUTUS_DB").map_err(|_| {
        "SYNTHEOS_PLUTUS_DB is required: set to a Postgres connection URL (e.g. postgres://user:pw@host/plutus)"
    })?;
    // Wrap PlutusStore in Arc before the trait-object coercion so the concrete handle
    // remains available for the operator bootstrap (create_org, add_member).
    let plutus_store = Arc::new(PlutusStore::open(&plutus_url).await.map_err(|e| {
        format!("plutus store open failed: {e}")
    })?);
    plutus_store.bootstrap_operator_org_if_absent().await.map_err(|e| {
        format!("plutus operator bootstrap: {e}")
    })?;
    let plutus: Arc<dyn PolicyBackend> = plutus_store.clone();
    // Log the redacted form only -- the raw URL carries the Postgres password in its userinfo
    // and must never reach the log stream (info-level logs routinely end up in aggregators/disk
    // with looser access control than the secret itself).
    tracing::info!(
        url = %redact_postgres_password(&plutus_url),
        "plutus policy authority open (real gate in plutus slot)"
    );

    // The dispatcher gate chain: all five slots now run REAL gates (pistis, plutus, eidolon,
    // human, phylax-when-keyed). The plutus slot is the real PlutusGate (Story 6.x / row 1).
    // The phylax credential authority is opt-in and fail-closed: it activates only when a master
    // key is configured (SYNTHEOS_PHYLAX_KEY). Absent a key, the phylax slot stays a deny-stub so
    // no credential operation is silently permitted.
    let phylax = phylax_from_env(bus.clone())?;
    match &phylax {
        Some(_) => {
            tracing::info!("phylax credential authority enabled (real gate in the phylax slot)")
        }
        None => tracing::info!("phylax disabled (SYNTHEOS_PHYLAX_KEY unset); phylax slot denies"),
    }

    // The pistis capability authority is a REAL gate in the pistis slot. Until live Matrix room
    // materialization lands, its room-state source is empty: a capability-bearing invocation fails
    // closed (no room state -> deny) while an invocation declaring no capability passes pistis for
    // the rest of the chain to decide.
    let pistis_source: Arc<dyn RoomStateSource> = Arc::new(InMemoryRoomStateSource::new());

    // The human-in-the-loop authority is a REAL gate in the human slot (Story 4.6).
    // Approval-required invocations block on this approver until a human decides
    // (via Rift, which calls RegistryApprover::resolve out-of-band) or the deadline
    // elapses and the gate denies (fail-closed). The deadline is configurable.
    let approval_timeout_secs = std::env::var("SYNTHEOS_HUMAN_APPROVAL_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    let human_approver: Arc<dyn Approver> =
        Arc::new(RegistryApprover::new(Duration::from_secs(approval_timeout_secs)));

    let policy = EidolonPolicy::default();
    let dispatcher = Arc::new(
        Dispatcher::new(
            live_gate_chain(
                &policy,
                thymus.clone(),
                pistis_source,
                phylax,
                bus.clone(),
                human_approver,
                plutus,
            )?,
            Box::new(DenyExecutor),
            bus.clone(),
        )?
        .with_output_filter(Box::new(EidolonOutputFilter::new(&policy)?)),
    );

    // The eidolon supervisor task (Stories 2.5/2.6): watches session JSONL and publishes
    // violation events on the shared bus. Opt-in and all-or-nothing: it runs only when the
    // watch dir AND the identity its events carry are explicitly configured -- a supervisor
    // with a fabricated identity would poison the audit trail.
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

    // The in-process cognitive core (Wave 2/3). Feature-gated: the default build
    // never constructs it. The lite session has no embedder and no background
    // loops -- the runtime composition of "kleos within Henosis without the whole
    // stack".
    //
    // PERSISTENT + WIRED (Wave 3): opened over a path-backed store
    // (`SYNTHEOS_COGNITION_DB`, default `data/cognition.db`), so stored memory
    // survives a restart, and the `/cognition/memory*` routes read it. The parent
    // directory is created on boot. The facade surface is still partial (see
    // scripts/known-incomplete.md row 3).
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
        dispatcher,
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

    // Mount the operator surface when SYNTHEOS_OPERATOR_JWT_SECRET is configured.
    // A set-but-invalid secret is a hard boot error so misconfiguration is never silent.
    if let Some(op_state) = operator_state_from_env(
        directory_store.clone(),
        plutus_store.clone(),
        soma.clone(),
        chiasm.clone(),
        broca.clone(),
        thymus.clone(),
        loom.clone(),
        bus.clone(),
    )
    .await?
    {
        // Bootstrap the first operator account when the bootstrap env vars are set.
        bootstrap_operator_if_configured(&directory_store, &plutus_store).await?;
        state = state.with_operator(op_state);
        tracing::info!("operator surface enabled: /api/auth/*, /api/dashboard, /ws");
    }

    // Mount the Stripe billing webhook when SYNTHEOS_STRIPE_WEBHOOK_SECRET is configured.
    // A set-but-empty secret is a hard boot error so misconfiguration is never silent.
    if let Some(billing_state) = billing_state_from_env(plutus_store.clone())? {
        state = state.with_billing(billing_state);
        tracing::info!("stripe billing webhook enabled: POST /billing/stripe/webhook");
    }

    // Resource limits around the whole surface: cap the body size, time out slow requests, and
    // bound how many run concurrently.
    let app = router(state)
        .layer(GlobalConcurrencyLimitLayer::new(MAX_IN_FLIGHT))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES));

    let addr = std::env::var("SYNTHEOS_ADDR").unwrap_or_else(|_| "127.0.0.1:8088".to_string());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "syntheos-server listening (all five gate slots real: pistis, plutus, eidolon, human, phylax-when-keyed)");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Build the supervisor from the environment, when enabled.
///
/// `SYNTHEOS_SUPERVISOR_WATCH_DIR` unset = disabled (`Ok(None)`). When set, the identity the
/// violation events carry is REQUIRED (`SYNTHEOS_SUPERVISOR_TENANT` /
/// `SYNTHEOS_SUPERVISOR_PRINCIPAL`, canonical UUID strings) and a configured-but-unreadable
/// rules file (`SYNTHEOS_SUPERVISOR_RULES`) is a boot error rather than a silent fall-back to
/// defaults. `SYNTHEOS_SUPERVISOR_ALLOWED_PATHS` (colon-separated) enables the file-scope check.
/// Open the Phylax credential store if a master key is configured, else `None`.
///
/// Opt-in and fail-closed: the store activates only when `SYNTHEOS_PHYLAX_KEY` holds a 64-hex
/// (32-byte) master key. The DB path defaults to `data/phylax.sqlite` (override with
/// `SYNTHEOS_PHYLAX_DB`). A set-but-malformed key is a hard boot error -- never a silent
/// fallback to no authority -- so a misconfiguration cannot quietly leave the slot denying when
/// the operator intended it enabled.
fn phylax_from_env(
    bus: Arc<AxonBus>,
) -> Result<Option<Arc<PhylaxStore>>, Box<dyn std::error::Error>> {
    let key_hex = match std::env::var("SYNTHEOS_PHYLAX_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return Ok(None),
    };
    let key_bytes =
        hex::decode(key_hex.trim()).map_err(|e| format!("SYNTHEOS_PHYLAX_KEY must be hex: {e}"))?;
    let key: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "SYNTHEOS_PHYLAX_KEY must decode to exactly 32 bytes (64 hex chars)")?;
    let db = db_path("SYNTHEOS_PHYLAX_DB", "data/phylax.sqlite")?;
    let store = PhylaxStore::open(&db, bus, key)?;
    tracing::info!(path = %db, "phylax credential store open");
    Ok(Some(Arc::new(store)))
}

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
        Ok(path) if !path.is_empty() => {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("SYNTHEOS_SUPERVISOR_RULES {path:?}: {e}"))?;
            supervisor::rules_from_json(&content)?
        }
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
    plutus_store: Arc<PlutusStore>,
) -> Result<Option<BillingState>, Box<dyn std::error::Error>> {
    let raw = match std::env::var("SYNTHEOS_STRIPE_WEBHOOK_SECRET") {
        Ok(secret) => Some(secret),
        Err(std::env::VarError::NotPresent) => None,
        Err(e) => return Err(format!("SYNTHEOS_STRIPE_WEBHOOK_SECRET: {e}").into()),
    };
    Ok(validated_webhook_secret(raw)?.map(|secret| BillingState::new(plutus_store, secret)))
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
    plutus_store: Arc<PlutusStore>,
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
    let secret_bytes: Vec<u8> =
        if raw.len() % 2 == 0 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
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

/// Bootstrap the first operator account when the bootstrap environment variables
/// are present and the account does not yet exist.
///
/// Reads:
/// - `SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL` -- the email address to create.
/// - `SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD` -- the plaintext password to hash.
///
/// When both are set AND no account for that email exists yet:
/// 1. Enroll a new `Human` principal in the identity directory.
/// 2. Create a Plutus org (`Enterprise` tier) with the new principal as `Owner`.
/// 3. Create the operator account in the identity store (argon2id-hashed password).
///
/// Idempotent: if the account already exists the whole block is skipped.
/// On a second boot the account row is present; no duplicate enroll or org creation.
///
/// This function is exercised by the manual launch checklist (Plan B), not a unit test.
async fn bootstrap_operator_if_configured(
    directory: &SqliteDirectory,
    plutus_store: &PlutusStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let email = match std::env::var("SYNTHEOS_OPERATOR_BOOTSTRAP_EMAIL") {
        Ok(e) if !e.is_empty() => e,
        _ => return Ok(()), // bootstrap env var not set; nothing to do
    };
    let password = match std::env::var("SYNTHEOS_OPERATOR_BOOTSTRAP_PASSWORD") {
        Ok(p) if !p.is_empty() => p,
        _ => return Ok(()), // bootstrap env var not set; nothing to do
    };

    // Idempotency check: if the account already exists, skip everything.
    if directory
        .get_account(&email)
        .map_err(|e| format!("bootstrap get_account: {e}"))?
        .is_some()
    {
        tracing::debug!(email, "operator account already exists; skipping bootstrap");
        return Ok(());
    }

    // 1. Enroll a new principal for this operator account.
    let principal = directory
        .enroll(PrincipalKind::Human, Some(format!("operator:{email}")))
        .await
        .map_err(|e| format!("bootstrap enroll: {e}"))?;

    // 2. Create a Plutus org with the new principal as Owner.
    //    A fresh TenantId is generated; it is logged so the operator knows their org UUID.
    let tenant = TenantId::new();
    plutus_store
        .create_org(tenant, "operator", principal.id, QuotaTier::Enterprise)
        .await
        .map_err(|e| format!("bootstrap create_org: {e}"))?;

    // 3. Create the account in the SQLite accounts store (argon2id hash applied inside).
    directory
        .create_account(&email, &password, principal.id)
        .map_err(|e| format!("bootstrap create_account: {e}"))?;

    tracing::info!(
        email,
        principal_id = %principal.id,
        tenant_id    = %tenant,
        "operator bootstrap complete: account created (Owner in new Enterprise org)"
    );

    Ok(())
}

/// Resolve a service database path from `var` (default `default`), creating the parent
/// directory if absent so `Connection::open` can create the file.
fn db_path(var: &str, default: &str) -> Result<String, std::io::Error> {
    let path = std::env::var(var).unwrap_or_else(|_| default.to_string());
    if let Some(parent) = std::path::Path::new(&path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    Ok(path)
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
        let redacted = redact_postgres_password("postgres://plutus:hunter2@db.internal:5432/plutus");
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
