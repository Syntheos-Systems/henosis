use futures::StreamExt;
use std::io::{self, BufRead, Write};
use std::pin::Pin;
use std::sync::Arc;

use synapse_core::{
    AgentConfig, AgentEvent, ConversationContext, ModelRouter, PricingTable, agent_loop,
    agent_turn_with_pricing,
};
use synapse_provider::{
    ChatRequest, ChatResponse, Provider, ProviderConfig, StreamEvent, create_provider,
};
use synapse_session::SessionStore;
use synapse_tools::default_tools;

/// Load `~/.synapse/hooks.toml` into a `HookConfig`. Missing file -> empty
/// config. Parse errors are logged but do not abort startup; the agent
/// continues without hooks rather than refusing to launch on a typo.
fn load_hooks_config() -> std::sync::Arc<synapse_core::HookConfig> {
    let Some(home) = dirs::home_dir() else {
        return std::sync::Arc::new(synapse_core::HookConfig::default());
    };
    let path = home.join(".synapse").join("hooks.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return std::sync::Arc::new(synapse_core::HookConfig::default());
        }
        Err(e) => {
            eprintln!("{YELLOW}warning: failed to read hooks.toml: {e}{RESET}");
            return std::sync::Arc::new(synapse_core::HookConfig::default());
        }
    };
    match toml::from_str::<synapse_core::HookConfig>(&text) {
        Ok(cfg) => {
            eprintln!(
                "{DIM}hooks: loaded {} hook(s) from {}{RESET}",
                cfg.hooks.len(),
                path.display()
            );
            std::sync::Arc::new(cfg)
        }
        Err(e) => {
            eprintln!("{YELLOW}warning: hooks.toml parse error: {e}{RESET}");
            std::sync::Arc::new(synapse_core::HookConfig::default())
        }
    }
}

/// Load a key from env var, then fall back to ~/.synapse/config.json field.
fn config_key(env_var: &str, json_field: &str) -> String {
    if let Ok(val) = std::env::var(env_var)
        && !val.is_empty()
    {
        return val;
    }
    let config_path = dirs::home_dir()
        .map(|h| h.join(".synapse").join("config.json"))
        .unwrap_or_default();
    if let Ok(data) = std::fs::read_to_string(&config_path)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&data)
        && let Some(s) = v.get(json_field).and_then(|v| v.as_str())
    {
        return s.to_owned();
    }
    String::new()
}

/// Collapse accepted provider aliases onto the canonical ids used by config and routing.
fn canonical_provider_name(name: &str) -> &str {
    match name {
        "codex" => "openai-codex",
        "zen" | "opencode" => "opencode-zen",
        other => other,
    }
}

/// Resolve provider autodetect precedence without depending on filesystem or env state.
fn autodetected_provider(
    has_codex_ready: bool,
    has_zen_auth: bool,
    has_anthropic_key: bool,
) -> &'static str {
    if has_codex_ready {
        "openai-codex"
    } else if has_zen_auth {
        "opencode-zen"
    } else if has_anthropic_key {
        "anthropic"
    } else {
        "proxy"
    }
}

/// Decide whether switching to OpenAI Codex should replace the current model with its default.
fn should_reset_openai_codex_model(model: &str) -> bool {
    model.is_empty()
        || model.starts_with("claude")
        || model.starts_with("qwen2")
        || model.starts_with("ri.language-model-service..")
        || synapse_provider::opencode_zen::MODEL_PRESETS.contains(&model)
}

/// Pick the model `synapse doctor` should probe for the OpenAI Codex provider.
fn doctor_openai_codex_model(configured_model: Option<&str>) -> String {
    configured_model
        .filter(|model| !model.is_empty())
        .unwrap_or(synapse_provider::openai_codex::DEFAULT_MODEL)
        .to_string()
}

/// Auto-detect the default provider when settings do not explicitly select one.
fn autodetect_default_provider() -> &'static str {
    let has_codex_ready = matches!(
        synapse_provider::openai_codex::CodexAuth::from_path(
            synapse_provider::openai_codex::CodexAuth::default_path(),
        )
        .status(),
        Ok(synapse_provider::openai_codex::AuthStatus::Ready { .. })
    );
    let has_zen_auth = !config_key("SYNAPSE_OPENCODE_KEY", "opencode_zen_key").is_empty()
        || synapse_provider::opencode_zen::load_subscription_token().is_some();
    let has_anthropic_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .is_some_and(|value| !value.is_empty());
    autodetected_provider(has_codex_ready, has_zen_auth, has_anthropic_key)
}

// The base spine plus untrusted-data rules now live in
// `synapse_core::system_prompt::DEFAULT_BASE_SPINE` and
// `DEFAULT_UNTRUSTED_RULES`. `SystemPromptBuilder::with_default_base()`
// composes them at startup.

/// Compact system prompt for local models. Keeps context small for faster inference.
const OLLAMA_SYSTEM_PROMPT: &str = "\
You are Synapse, a local coding agent. Execute tasks directly. Be concise.

You have tools: bash (run commands), read/write/edit (files), glob/grep (search).
Work in the current directory. No hedging, no asking permission.

Focus: git operations, file management, code tasks. Run commands and report results.
";

const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Persist a single LLM-request usage event to the SQLite `usage` table.
///
/// Replaces the prior `~/.synapse/usage.jsonl` append-only log so `/cost`,
/// future analytics, and the Eidolon activity surface can aggregate spend
/// with a SQL query instead of a line-by-line parse. Cache read/write token
/// counts are propagated from the provider stream and persisted alongside the
/// full token counts.
#[allow(clippy::too_many_arguments)]
fn log_usage(
    store: Option<&SessionStore>,
    session_id: Option<i64>,
    model: &str,
    provider: &str,
    input_tokens: u32,
    output_tokens: u32,
    cache_read_tokens: u32,
    cache_write_tokens: u32,
    usd: f64,
) {
    let Some(store) = store else { return };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let rec = synapse_session::UsageRecord {
        timestamp: now,
        session_id,
        model: model.to_string(),
        provider: provider.to_string(),
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        cost_usd: usd,
    };
    if let Err(e) = store.insert_usage(&rec) {
        log::warn!("failed to record usage: {e}");
    }
}

/// Newtype wrapper to satisfy Arc<dyn Provider + Send + Sync> from Box<dyn Provider>.
struct ProviderWrapper(Box<dyn Provider>);

/// Forward provider trait calls to the boxed provider implementation.
#[async_trait::async_trait]
impl Provider for ProviderWrapper {
    /// Send a one-shot chat request through the wrapped provider.
    async fn send(&self, request: &ChatRequest) -> anyhow::Result<ChatResponse> {
        self.0.send(request).await
    }

    /// Send a streaming chat request through the wrapped provider.
    fn send_streaming(
        &self,
        request: &ChatRequest,
    ) -> Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>> {
        self.0.send_streaming(request)
    }

    /// Report the wrapped provider name for telemetry and UI output.
    fn name(&self) -> &str {
        self.0.name()
    }
}

/// Accumulates streaming assistant text and, at turn end, re-renders the
/// buffered content through `termimad` when markdown features are detected.
///
/// Two-pass UX: raw deltas stream live so the user sees activity, then on
/// `TurnEnd` we emit a `--- rendered ---` separator followed by the polished
/// markdown render. Plain-text replies skip the second pass entirely so the
/// terminal stays uncluttered.
struct MarkdownBuffer {
    /// Accumulated assistant text for the current turn.
    buf: String,
}

/// Manage buffered markdown state for the two-pass terminal renderer.
impl MarkdownBuffer {
    /// Create an empty buffer.
    fn new() -> Self {
        Self { buf: String::new() }
    }

    /// Append a streaming text delta. Does not emit anything by itself; the
    /// CLI prints the raw delta separately for the streaming feel.
    fn push(&mut self, delta: &str) {
        self.buf.push_str(delta);
    }

    /// Reset the buffer between turns.
    fn clear(&mut self) {
        self.buf.clear();
    }

    /// Returns true when the accumulated text contains markdown structural
    /// features worth re-rendering. Cheap heuristic; avoids pulling a parser
    /// just to detect "do we have anything to render".
    fn has_markdown(&self) -> bool {
        let s = self.buf.as_str();
        // Code fence is the strongest signal -- raw text never includes it.
        if s.contains("```") {
            return true;
        }
        // Headers, lists, tables, blockquotes, inline code, bold/italic markers.
        for line in s.lines() {
            let t = line.trim_start();
            if t.starts_with('#')
                || t.starts_with("- ")
                || t.starts_with("* ")
                || t.starts_with("> ")
                || t.starts_with("| ")
                || (t.len() >= 2 && t.starts_with(char::is_numeric) && t.contains(". "))
            {
                return true;
            }
        }
        s.contains("**") || s.contains("`") || (s.contains('[') && s.contains("]("))
    }

    /// Emit a separator and the rendered markdown if features are present.
    /// Called from `run_event_loop` on `TurnEnd`. No-op for plain text.
    fn render_if_markdown(&self) {
        if !self.has_markdown() || self.buf.trim().is_empty() {
            return;
        }
        let skin = termimad::MadSkin::default();
        println!("\n{DIM}--- rendered ---{RESET}");
        // termimad's `print_text` writes the formatted output to stdout.
        skin.print_text(&self.buf);
    }
}

/// Maximum bytes of a non-diff tool result to display inline before truncating.
/// Diff-producing tools (edit, write) bypass this cap so the full patch renders.
const TOOL_RESULT_DISPLAY_CAP_BYTES: usize = 4096;

/// Tools whose results should render in full without size capping.
/// These produce structured output (diffs) where truncation destroys signal.
const DIFF_PRODUCING_TOOLS: &[&str] = &["edit", "write"];

/// Colorize a unified-diff body so + and - lines are visible at a glance.
/// Header lines (---, +++, @@) get cyan; insertions green; deletions red.
fn colorize_diff(content: &str) -> String {
    let mut out = String::with_capacity(content.len() + 64);
    for line in content.lines() {
        if line.starts_with("---") || line.starts_with("+++") || line.starts_with("@@") {
            out.push_str(CYAN);
            out.push_str(line);
            out.push_str(RESET);
        } else if line.starts_with('+') {
            out.push_str(GREEN);
            out.push_str(line);
            out.push_str(RESET);
        } else if line.starts_with('-') {
            out.push_str(RED);
            out.push_str(line);
            out.push_str(RESET);
        } else {
            out.push_str(DIM);
            out.push_str(line);
            out.push_str(RESET);
        }
        out.push('\n');
    }
    out
}

/// Truncate `s` to at most `cap` bytes on a UTF-8 char boundary,
/// appending a "...[truncated N bytes]" suffix when material was cut.
fn truncate_at_char_boundary(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    format!("{}\n...[truncated {} bytes]", &s[..end], dropped)
}

/// Render the agent event stream into Synapse's terminal UI.
async fn run_event_loop(
    mut stream: Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>>,
    session_id: Option<i64>,
    session_store: Option<Arc<SessionStore>>,
    provider_name: &str,
) {
    // Track tool name by id so ToolResult (which only carries id) can be
    // rendered per-tool-type. ToolStart populates; ToolResult consumes.
    let mut tool_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Accumulates assistant text for end-of-turn markdown rendering.
    let mut md = MarkdownBuffer::new();

    while let Some(event) = stream.next().await {
        match event {
            AgentEvent::TurnStart => {
                println!("\n{DIM}--- turn ---{RESET}");
                md.clear();
            }
            AgentEvent::Text(t) => {
                print!("{t}");
                md.push(&t);
            }
            AgentEvent::ToolStart { id, name } => {
                print!("\n{DIM}[tool: {name}]{RESET}");
                tool_names.insert(id, name);
            }
            AgentEvent::ToolResult {
                id,
                content,
                is_error,
            } => {
                let name = tool_names.remove(&id).unwrap_or_default();
                let is_diff_tool = DIFF_PRODUCING_TOOLS.contains(&name.as_str());

                if is_error {
                    // Errors always get red. Cap to avoid filling the terminal with stack traces.
                    let body = truncate_at_char_boundary(&content, TOOL_RESULT_DISPLAY_CAP_BYTES);
                    println!("\n{RED}{body}{RESET}");
                } else if is_diff_tool {
                    // Render the full diff with +/- line colorization. No truncation:
                    // a clipped patch is worse than a long one because it hides changes.
                    println!("\n{}", colorize_diff(&content));
                } else {
                    // Non-diff success result: dim, capped at 4 KB.
                    let body = truncate_at_char_boundary(&content, TOOL_RESULT_DISPLAY_CAP_BYTES);
                    println!("\n{DIM}{body}{RESET}");
                }
            }
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens: _, // write count is persisted via the Cost arm, not shown per-turn
            } => {
                if cache_read_tokens > 0 && input_tokens > 0 {
                    let pct = (cache_read_tokens as f64 / input_tokens as f64 * 100.0)
                        .round()
                        .clamp(0.0, 100.0) as u32;
                    print!(
                        "\n{DIM}[tokens: in={input_tokens} out={output_tokens} (cached {cache_read_tokens}, {pct}%)]{RESET}"
                    );
                } else {
                    print!("\n{DIM}[tokens: in={input_tokens} out={output_tokens}]{RESET}");
                }
            }
            AgentEvent::Cost {
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                turn_usd,
                session_total_usd,
            } => {
                print!(" {GREEN}[turn ${turn_usd:.4} | session ${session_total_usd:.4}]{RESET}");
                log_usage(
                    session_store.as_deref(),
                    session_id,
                    &model,
                    provider_name,
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    turn_usd,
                );
            }
            AgentEvent::ModelSwitch { from: _, to } => {
                print!("\n{DIM}[model: {to}]{RESET}");
            }
            AgentEvent::TurnEnd => {
                println!();
                md.render_if_markdown();
                md.clear();
            }
            AgentEvent::Error(e) => {
                eprintln!("\n{RED}error: {e}{RESET}");
            }
        }
    }
}

/// Outcome of `handle_registry_outcome` -- tells the REPL whether the
/// dispatched command consumed the turn, ended the session, or should
/// queue text for the next agent turn.
enum RegistryDispatch {
    /// Command handled itself; the REPL should `continue` to the next prompt.
    Handled,
    /// Command requested the session end. REPL breaks.
    Exit,
    /// Command produced text that should be sent to the LLM as if the
    /// user had typed it. The carried `String` becomes the next prompt.
    /// No producer in v1; reserved for `/loop` and future async-prompt
    /// commands that need to drive the next turn.
    #[allow(dead_code)]
    Queue(String),
}

/// Apply a `CommandOutcome` returned by the registry. Handles
/// SwitchPersona by reloading the persona and rebuilding the prompt;
/// SwitchModel by mutating the active config; Exit/Noop/Message/Error
/// by printing or signaling the REPL loop.
async fn handle_registry_outcome(
    outcome: synapse_core::CommandOutcome,
    config: &mut synapse_core::AgentConfig,
    prompt_builder: &mut synapse_core::SystemPromptBuilder,
    ctx: &Arc<tokio::sync::Mutex<ConversationContext>>,
    startup_recall_blocks: &[String],
    active_persona_name: &mut Option<String>,
) -> RegistryDispatch {
    use synapse_core::CommandOutcome::*;
    match outcome {
        Exit => RegistryDispatch::Exit,
        Noop => RegistryDispatch::Handled,
        Message(s) => {
            println!("{s}");
            RegistryDispatch::Handled
        }
        Error(s) => {
            eprintln!("{RED}{s}{RESET}");
            RegistryDispatch::Handled
        }
        Queue(text) => RegistryDispatch::Queue(text),
        ClearContext { system_prompt } => {
            let mut c = ctx.lock().await;
            c.set_system(system_prompt.clone());
            config.system_prompt = system_prompt;
            println!("{DIM}Context cleared.{RESET}");
            RegistryDispatch::Handled
        }
        SwitchModel { provider, model } => {
            // We don't rebuild the Provider here -- that requires a
            // re-run of provider_config + create_provider with the new
            // settings. Phase 1's surface only mutates the model
            // string; the user can /settings to fully swap providers.
            if let Some(p) = provider {
                eprintln!("{DIM}provider swap requested ({p}); use /settings to apply{RESET}");
            }
            if let Some(m) = model {
                config.model = m.clone();
                if let Some(ref mut r) = config.router {
                    r.primary = m.clone();
                }
                println!("{DIM}model -> {m}{RESET}");
            }
            RegistryDispatch::Handled
        }
        SwitchPersona(name) => {
            let opts = synapse_core::ResolverOptions::default();
            match synapse_core::load_by_name(&name, &opts) {
                Ok(Some(p)) => {
                    prompt_builder.with_persona(Some(&p));
                    // Re-merge startup recall so the persona swap
                    // doesn't strip prior context.
                    if !startup_recall_blocks.is_empty() {
                        prompt_builder.with_kleos_recall(startup_recall_blocks);
                    }
                    let rendered = prompt_builder.render();
                    {
                        let mut c = ctx.lock().await;
                        c.set_system(rendered.clone());
                    }
                    config.system_prompt = rendered;
                    *active_persona_name = Some(p.name.clone());
                    println!("{DIM}persona -> {}{RESET}", p.name);
                }
                Ok(None) => {
                    eprintln!("{RED}persona {name:?} not found{RESET}");
                }
                Err(e) => {
                    eprintln!("{RED}persona load failed: {e}{RESET}");
                }
            }
            RegistryDispatch::Handled
        }
    }
}

/// Prompt for one line of terminal input and trim the trailing newline.
fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    let mut line = String::new();
    match io::stdin().lock().read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => Some(line.trim().to_string()),
        Err(_) => None,
    }
}

/// Resolve PIV_PIN from cred vault if not already in the environment.
/// Mirrors the zshrc wrapper: `cred exec yubikey piv-pin --env PIV_PIN`.
fn ensure_piv_pin() {
    if std::env::var("PIV_PIN")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
    {
        return;
    }
    let output = std::process::Command::new("cred")
        .args([
            "exec",
            "yubikey",
            "piv-pin",
            "--env",
            "PIV_PIN",
            "--",
            "sh",
            "-c",
            "echo $PIV_PIN",
        ])
        .output();
    if let Ok(out) = output {
        let pin = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !pin.is_empty() {
            unsafe { std::env::set_var("PIV_PIN", &pin) };
        }
    }
}

/// Tighten filesystem permissions on `~/.synapse/` and its known
/// secrets-bearing files so other users on the host cannot read
/// transcripts, the SQLite DB, the config (which can hold API keys),
/// or the prior usage log. Best-effort: missing files are skipped,
/// errors are logged but never fatal. Only meaningful on Unix.
#[cfg(unix)]
fn harden_synapse_dir_perms() {
    use std::os::unix::fs::PermissionsExt;
    let Some(home) = dirs::home_dir() else { return };
    let root = home.join(".synapse");
    if !root.exists() {
        return;
    }

    // 0700 on the directory itself.
    if let Ok(meta) = std::fs::metadata(&root) {
        let mut perms = meta.permissions();
        perms.set_mode(0o700);
        let _ = std::fs::set_permissions(&root, perms);
    }

    // 0600 on the per-file secrets and state.
    for name in [
        "sessions.db",
        "sessions.db-wal",
        "sessions.db-shm",
        "config.json",
        "hooks.toml",
        "usage.jsonl",
    ] {
        let p = root.join(name);
        if let Ok(meta) = std::fs::metadata(&p) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            if let Err(e) = std::fs::set_permissions(&p, perms) {
                log::warn!("could not chmod 0600 {}: {e}", p.display());
            }
        }
    }
}

#[cfg(not(unix))]
/// Skip filesystem permission hardening on non-Unix platforms.
fn harden_synapse_dir_perms() {}

/// Probe the PIV identity at startup and emit a visible warning if the
/// signer cannot be built. A red warning means no signer at all (Kleos
/// requests will fall back to KLEOS_API_KEY/phylaxd, which still works but
/// loses non-repudiation for Broca audit entries). A yellow warning means
/// the YubiKey wasn't reachable but a file/env key is present.
///
/// Phase 8 will extend this with X.509 cert expiry parsing once the
/// YubiKey backend exposes `not_after` -- for v0.x the boolean "signer
/// present?" check is the highest-value signal.
fn piv_status_warning() {
    let host = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());
    match henosis_memory_client::RequestSigner::from_env_or_file(&host, "synapse", "local") {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!(
                "{YELLOW}warning: no PIV identity file/env found -- \
                 Broca audit entries will be unsigned. \
                 Run `kleos-cli identity enroll` to fix.{RESET}"
            );
        }
        Err(e) => {
            eprintln!(
                "{RED}error: PIV signer init failed: {e}.{RESET} \
                 Kleos requests will use the API key fallback if configured."
            );
        }
    }
}

/// Build a Kleos client with the same auth cascade as kleos-cli:
/// PIV YubiKey → KLEOS_API_KEY env → phylaxd bootstrap.
async fn bootstrap_kleos_client() -> henosis_memory_client::Client {
    ensure_piv_pin();
    let base_url =
        std::env::var("KLEOS_URL").unwrap_or_else(|_| "http://localhost:4200".to_string());
    let host = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let signer = henosis_memory_client::RequestSigner::from_env_or_file(&host, "synapse", "local")
        .ok()
        .flatten();
    let api_key = match std::env::var("KLEOS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
    {
        Some(k) => Some(k),
        None => {
            let slot = henosis_memory_client::bootstrap::current_agent_slot();
            henosis_memory_client::bootstrap::resolve_api_key(&slot)
                .await
                .ok()
        }
    };
    henosis_memory_client::Client::new(base_url, api_key, signer)
}

/// Complete OpenAI Codex browser OAuth and persist Synapse auth storage.
async fn run_openai_codex_login() -> anyhow::Result<()> {
    let auth = synapse_provider::openai_codex::CodexAuth::from_path(
        synapse_provider::openai_codex::CodexAuth::default_path(),
    );
    let (listener, redirect_uri) = bind_openai_codex_callback().await?;
    let pkce = synapse_provider::openai_codex::generate_pkce();
    let state = synapse_provider::openai_codex::generate_pkce().verifier;
    let url =
        synapse_provider::openai_codex::authorization_url(&redirect_uri, &state, &pkce.challenge);

    println!("{BOLD}{CYAN}OpenAI Codex login{RESET}");
    println!("{DIM}callback: {redirect_uri}{RESET}");
    println!("{DIM}auth file: {}{RESET}", auth.path().display());
    if open_browser(&url) {
        println!("{GREEN}Opened browser for ChatGPT sign-in.{RESET}");
    } else {
        println!("{YELLOW}Could not open a browser automatically. Open this URL:{RESET}");
        println!("{url}");
    }

    let code = wait_for_openai_codex_callback(listener, &state).await?;
    let client = reqwest::Client::builder()
        .user_agent("synapse/0.1.0")
        .build()
        .map_err(|error| anyhow::anyhow!("build OpenAI Codex OAuth client: {error}"))?;
    let entry = synapse_provider::openai_codex::exchange_code(
        &client,
        &code,
        &pkce.verifier,
        &redirect_uri,
    )
    .await?;
    synapse_provider::openai_codex::save_provider_entry(auth.path(), entry)?;

    println!("{GREEN}OpenAI Codex auth saved.{RESET}");
    Ok(())
}

/// Remove the OpenAI Codex provider entry from Synapse auth storage.
fn run_openai_codex_logout() -> anyhow::Result<()> {
    let auth = synapse_provider::openai_codex::CodexAuth::from_path(
        synapse_provider::openai_codex::CodexAuth::default_path(),
    );
    synapse_provider::openai_codex::remove_provider(auth.path())?;
    println!(
        "{GREEN}OpenAI Codex auth removed from {}.{RESET}",
        auth.path().display()
    );
    Ok(())
}

/// Print the current OpenAI Codex auth status without starting the agent loop.
fn run_openai_codex_auth_status() -> anyhow::Result<()> {
    let auth = synapse_provider::openai_codex::CodexAuth::from_path(
        synapse_provider::openai_codex::CodexAuth::default_path(),
    );
    println!("{BOLD}{CYAN}OpenAI Codex auth{RESET}");
    println!("{DIM}auth file: {}{RESET}", auth.path().display());
    match auth.status()? {
        synapse_provider::openai_codex::AuthStatus::Missing => {
            println!("{YELLOW}status: missing{RESET}");
            println!("{DIM}Run `synapse login openai-codex`.{RESET}");
        }
        synapse_provider::openai_codex::AuthStatus::RefreshNeeded => {
            println!("{YELLOW}status: refresh needed{RESET}");
            println!("{DIM}Run `synapse login openai-codex` to renew credentials.{RESET}");
        }
        synapse_provider::openai_codex::AuthStatus::Ready { expires_at } => {
            println!("{GREEN}status: ready{RESET}");
            println!("{DIM}expires_at: {expires_at}{RESET}");
            if let Some(entry) =
                synapse_provider::openai_codex::load_auth_file(auth.path())?.openai_codex()?
            {
                println!("{DIM}base url: {}{RESET}", entry.base_url);
                if let Some(account) = entry.account
                    && let Some(email) = account.email
                {
                    println!("{DIM}account: {email}{RESET}");
                }
            }
        }
    }
    Ok(())
}

/// Bind the localhost OAuth callback listener, falling back when the default port is busy.
async fn bind_openai_codex_callback() -> anyhow::Result<(tokio::net::TcpListener, String)> {
    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 1455)).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = error;
            tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|bind_error| {
                    anyhow::anyhow!("bind OpenAI Codex OAuth callback listener: {bind_error}")
                })?
        }
    };
    let port = listener
        .local_addr()
        .map_err(|error| anyhow::anyhow!("read OpenAI Codex callback address: {error}"))?
        .port();
    Ok((listener, format!("http://127.0.0.1:{port}/auth/callback")))
}

/// Package a callback parse failure with the browser response that should be returned.
#[derive(Debug)]
struct OpenAICodexCallbackError {
    /// HTTP status code for the tiny callback response page.
    http_status: u16,
    /// Short heading shown in the browser tab after the callback completes.
    browser_message: &'static str,
    /// Machine-facing error returned to the CLI caller.
    error: anyhow::Error,
}

/// Validate the callback query parameters and extract the authorization code.
fn parse_openai_codex_callback(
    path: &str,
    expected_state: &str,
) -> Result<String, OpenAICodexCallbackError> {
    let state = query_value(path, "state").unwrap_or_default();
    if state != expected_state {
        return Err(OpenAICodexCallbackError {
            http_status: 400,
            browser_message: "State mismatch",
            error: anyhow::anyhow!("OpenAI Codex OAuth state mismatch"),
        });
    }

    if let Some(error) =
        query_value(path, "error_description").or_else(|| query_value(path, "error"))
    {
        return Err(OpenAICodexCallbackError {
            http_status: 400,
            browser_message: "Login failed",
            error: anyhow::anyhow!("OpenAI Codex OAuth error: {error}"),
        });
    }

    let Some(code) = query_value(path, "code") else {
        return Err(OpenAICodexCallbackError {
            http_status: 400,
            browser_message: "Missing code",
            error: anyhow::anyhow!("OpenAI Codex OAuth callback missing code"),
        });
    };

    Ok(code)
}

/// Wait for the OpenAI Codex browser callback and extract the authorization code.
async fn wait_for_openai_codex_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt;

    let accept = async {
        let (mut socket, _) = listener.accept().await?;
        let mut buffer = vec![0_u8; 8192];
        let read = socket.read(&mut buffer).await?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");

        let code = match parse_openai_codex_callback(path, expected_state) {
            Ok(code) => code,
            Err(error) => {
                write_oauth_callback_response(
                    &mut socket,
                    error.http_status,
                    error.browser_message,
                )
                .await?;
                return Err(error.error);
            }
        };

        write_oauth_callback_response(&mut socket, 200, "OpenAI Codex login complete").await?;
        Ok::<_, anyhow::Error>(code)
    };

    tokio::time::timeout(std::time::Duration::from_secs(600), accept)
        .await
        .map_err(|error| anyhow::anyhow!("OpenAI Codex login timed out: {error}"))?
}

/// Write the tiny browser response page returned from the OAuth callback listener.
async fn write_oauth_callback_response(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let status_text = if status == 200 { "OK" } else { "Bad Request" };
    let html =
        format!("<html><body><h1>{message}</h1><p>You can close this tab.</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    socket.write_all(response.as_bytes()).await
}

/// Return a decoded query value from an HTTP callback request target.
fn query_value(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        let (candidate, value) = pair.split_once('=').unwrap_or((pair, ""));
        if candidate == key {
            let form_value = value.replace('+', " ");
            return urlencoding::decode(&form_value)
                .ok()
                .map(|value| value.into_owned());
        }
    }
    None
}

/// Try to launch the platform browser for the OAuth login URL.
fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        return std::process::Command::new("open").arg(url).spawn().is_ok();
    }
    #[cfg(target_os = "windows")]
    {
        return std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .is_ok();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        return ["xdg-open", "wslview"]
            .iter()
            .any(|program| std::process::Command::new(program).arg(url).spawn().is_ok());
    }
    #[allow(unreachable_code)]
    false
}

#[tokio::main]
/// Start the Synapse CLI, subcommands, or interactive REPL.
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Auth subcommands run before unrelated startup work so login/status/logout
    // do not emit Kleos or filesystem noise.
    if args.first().map(String::as_str) == Some("login")
        && args.get(1).map(String::as_str) == Some("openai-codex")
    {
        run_openai_codex_login().await?;
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("logout")
        && args.get(1).map(String::as_str) == Some("openai-codex")
    {
        run_openai_codex_logout()?;
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("auth")
        && args.get(1).map(String::as_str) == Some("status")
    {
        run_openai_codex_auth_status()?;
        return Ok(());
    }

    // Ensure Kleos env vars are set from config for tools to use
    let kleos_url_cfg = config_key("KLEOS_URL", "kleos_url");
    if !kleos_url_cfg.is_empty() {
        // SAFETY: called before any threads are spawned (single-threaded at this point)
        unsafe {
            std::env::set_var("KLEOS_URL", &kleos_url_cfg);
        }
    }
    let kleos_key_cfg = config_key("KLEOS_API_KEY", "kleos_api_key");
    if !kleos_key_cfg.is_empty() {
        unsafe {
            std::env::set_var("KLEOS_API_KEY", &kleos_key_cfg);
        }
    }

    // Surface PIV signer status early so the user sees one warning at the
    // top of the session rather than discovering it mid-tool-call.
    piv_status_warning();

    // Tighten permissions on the persisted state directory so other users
    // on shared machines cannot read transcripts, config, or tokens.
    harden_synapse_dir_perms();

    // Pre-scan for --provider so subcommands like `doctor` can use it.
    let mut prescan_provider: Option<&str> = None;
    for (i, arg) in args.iter().enumerate() {
        if (arg == "--provider" || arg == "-p") && i + 1 < args.len() {
            prescan_provider = Some(&args[i + 1]);
            break;
        }
    }

    // Subcommand: doctor -- diagnose provider config + connectivity, then exit.
    if args.iter().any(|s| s == "doctor") {
        let code = run_doctor(prescan_provider).await;
        std::process::exit(code);
    }

    // Resolve default provider from settings or auto-detection.
    let settings_provider = load_settings();
    let default_provider: &str =
        if let Some(p) = settings_provider.get("provider").and_then(|v| v.as_str()) {
            // Leak into 'static -- settings_provider is only read once at startup.
            Box::leak(canonical_provider_name(p).to_string().into_boxed_str())
        } else {
            autodetect_default_provider()
        };
    let mut provider_type = default_provider;
    let mut model: Option<String> = None;
    let mut message_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--provider" | "-p" => {
                provider_type = canonical_provider_name(&args[i + 1]);
                i += 2;
            }
            "--model" | "-m" => {
                model = Some(args[i + 1].clone());
                i += 2;
            }
            _ => {
                message_parts.push(args[i].clone());
                i += 1;
            }
        }
    }
    let initial_message = message_parts.join(" ");

    // Standard provider mode
    let is_ollama = provider_type == "ollama";
    let default_model = model.unwrap_or_else(|| {
        // Prefer config-set model if any.
        let cfg_model = config_key("SYNAPSE_MODEL", "model");
        if !cfg_model.is_empty() {
            return cfg_model;
        }
        if is_ollama {
            "qwen2.5:7b".to_string()
        } else if provider_type == "openai-codex" {
            synapse_provider::openai_codex::DEFAULT_MODEL.to_string()
        } else if provider_type == "opencode-zen" {
            synapse_provider::opencode_zen::DEFAULT_MODEL.to_string()
        } else if provider_type == "foundry-openai" {
            "ri.language-model-service..language-model.gpt-4-1".to_string()
        } else if provider_type == "foundry-anthropic" {
            "ri.language-model-service..language-model.anthropic-claude-4-6-sonnet".to_string()
        } else {
            "claude-sonnet-4-20250514".to_string()
        }
    });

    // Load fast model for multi-model routing (turn 2+ uses this cheaper model)
    let fast_model = {
        let v = config_key("SYNAPSE_FAST_MODEL", "fast_model");
        if v.is_empty() { None } else { Some(v) }
    };

    // Build model router if fast model is configured
    let router = fast_model
        .as_ref()
        .map(|fast| ModelRouter::new(default_model.clone()).with_fast(fast.clone()));

    let provider_config = match provider_type {
        "anthropic" => ProviderConfig::AnthropicAuto,
        "openai-codex" | "codex" => {
            let base_url = {
                let v = config_key("SYNAPSE_OPENAI_CODEX_URL", "openai_codex_url");
                if v.is_empty() { None } else { Some(v) }
            };
            ProviderConfig::OpenAICodexAuto {
                auth_path: None,
                base_url,
            }
        }
        "ollama" => {
            let base_url = {
                let v = config_key("SYNAPSE_OLLAMA_URL", "ollama_url");
                if v.is_empty() { None } else { Some(v) }
            };
            ProviderConfig::Ollama { base_url }
        }
        "proxy" => {
            let base_url = {
                let v = config_key("SYNAPSE_PROXY_URL", "openai_base_url");
                if v.is_empty() {
                    eprintln!("{RED}SYNAPSE_PROXY_URL not set{RESET}");
                    std::process::exit(1)
                } else {
                    v
                }
            };
            let api_key = {
                let v = config_key("SYNAPSE_PROXY_KEY", "openai_api_key");
                if v.is_empty() {
                    eprintln!("SYNAPSE_PROXY_KEY not set");
                    std::process::exit(1);
                }
                v
            };
            ProviderConfig::Proxy { base_url, api_key }
        }
        "opencode-zen" | "zen" | "opencode" => {
            let base_url = {
                let v = config_key("SYNAPSE_OPENCODE_URL", "opencode_zen_url");
                if v.is_empty() { None } else { Some(v) }
            };
            // Priority: explicit env/config key > OpenCode CLI subscription token from auth.json.
            let explicit_key = config_key("SYNAPSE_OPENCODE_KEY", "opencode_zen_key");
            if !explicit_key.is_empty() {
                ProviderConfig::OpenCodeZen {
                    api_key: explicit_key,
                    base_url,
                }
            } else if synapse_provider::opencode_zen::load_subscription_token().is_some() {
                eprintln!("{DIM}opencode-zen: using subscription token from auth.json{RESET}");
                ProviderConfig::OpenCodeZenAuto { base_url }
            } else {
                eprintln!(
                    "{RED}No OpenCode Zen credential found.{RESET}\n\
                     Either log in with `opencode providers` (writes auth.json),\n\
                     set SYNAPSE_OPENCODE_KEY in env, or set \"opencode_zen_key\" in ~/.synapse/config.json."
                );
                std::process::exit(1);
            }
        }
        "azure" => {
            let endpoint = config_key("SYNAPSE_AZURE_ENDPOINT", "azure_endpoint");
            let deployment = config_key("SYNAPSE_AZURE_DEPLOYMENT", "azure_deployment");
            let api_key = config_key("SYNAPSE_AZURE_KEY", "azure_api_key");
            if endpoint.is_empty() || deployment.is_empty() || api_key.is_empty() {
                eprintln!(
                    "{RED}Azure requires endpoint, deployment, and api_key.{RESET}\n\
                     Set SYNAPSE_AZURE_ENDPOINT, SYNAPSE_AZURE_DEPLOYMENT, SYNAPSE_AZURE_KEY,\n\
                     or fields azure_endpoint / azure_deployment / azure_api_key in config.json."
                );
                std::process::exit(1);
            }
            let api_version = {
                let v = config_key("SYNAPSE_AZURE_API_VERSION", "azure_api_version");
                if v.is_empty() { None } else { Some(v) }
            };
            ProviderConfig::Azure {
                endpoint,
                deployment,
                api_key,
                api_version,
            }
        }
        "foundry-openai" => {
            let host = config_key("SYNAPSE_FOUNDRY_HOST", "foundry_host");
            let token = config_key("SYNAPSE_FOUNDRY_TOKEN", "foundry_token");
            if host.is_empty() || token.is_empty() {
                eprintln!(
                    "{RED}Foundry requires host and token.{RESET}\n\
                     Set SYNAPSE_FOUNDRY_HOST + SYNAPSE_FOUNDRY_TOKEN,\n\
                     or \"foundry_host\" + \"foundry_token\" in ~/.synapse/config.json."
                );
                std::process::exit(1);
            }
            ProviderConfig::FoundryOpenAI { host, token }
        }
        "foundry-anthropic" => {
            let host = config_key("SYNAPSE_FOUNDRY_HOST", "foundry_host");
            let token = config_key("SYNAPSE_FOUNDRY_TOKEN", "foundry_token");
            if host.is_empty() || token.is_empty() {
                eprintln!(
                    "{RED}Foundry requires host and token.{RESET}\n\
                     Set SYNAPSE_FOUNDRY_HOST + SYNAPSE_FOUNDRY_TOKEN,\n\
                     or \"foundry_host\" + \"foundry_token\" in ~/.synapse/config.json."
                );
                std::process::exit(1);
            }
            ProviderConfig::FoundryAnthropic { host, token }
        }
        other => {
            eprintln!("Unknown provider: {other}");
            std::process::exit(1);
        }
    };

    let provider: Arc<dyn synapse_provider::Provider + Send + Sync> =
        Arc::new(ProviderWrapper(create_provider(provider_config)?));

    // Build tool registry -- delegate_task is added after config is built (needs provider + config)
    let mut tool_registry = default_tools();

    // Bootstrap: resolve persona, build system prompt via the layered
    // SystemPromptBuilder, and fold in Kleos recall + skill index.
    let cwd_for_persona = std::env::current_dir()?;
    let persona_opts = synapse_core::ResolverOptions::default();
    let mut resolved_persona = match synapse_core::resolve(&cwd_for_persona, &persona_opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{DIM}persona resolve failed: {e}{RESET}");
            None
        }
    };

    // Automate mode: if `frameshift automate status` reports on, ask the
    // Frameshift ranker which persona fits the initial task. This trumps
    // any earlier resolver hit -- the engine's per-project automate flag
    // is the user's standing instruction to let the agent self-select.
    let automate = synapse_core::automate_status();
    let automate_opts = synapse_core::AutomateOptions::default();
    if automate.on {
        let task_summary: String = if !initial_message.is_empty() {
            initial_message.chars().take(280).collect()
        } else {
            cwd_for_persona
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "synapse session".into())
        };
        match synapse_core::select_for_task(&task_summary, &automate_opts) {
            Ok(Some(pick)) => match synapse_core::load_by_name(&pick.name, &persona_opts) {
                Ok(Some(p)) => {
                    eprintln!(
                        "{DIM}automate: -> {} (score {:.2}, margin {:.2}){RESET}",
                        p.name, pick.score, pick.margin
                    );
                    resolved_persona = Some(p);
                }
                Ok(None) => eprintln!(
                    "{DIM}automate: top pick {} not loadable, holding{RESET}",
                    pick.name
                ),
                Err(e) => eprintln!("{DIM}automate: load failed: {e}{RESET}"),
            },
            Ok(None) => eprintln!("{DIM}automate: no confident pick, holding{RESET}"),
            Err(e) => eprintln!("{DIM}automate: select failed: {e}{RESET}"),
        }
    } else if let Some(active) = automate.active.as_deref()
        && resolved_persona.is_none()
    {
        // Automate is off, but the engine remembers a previous explicit
        // active persona. Honor it as a sticky default.
        if let Ok(Some(p)) = synapse_core::load_by_name(active, &persona_opts) {
            eprintln!("{DIM}persona: {} (engine-active){RESET}", p.name);
            resolved_persona = Some(p);
        }
    }

    if let Some(ref p) = resolved_persona {
        let src = match p.source {
            synapse_core::ResolutionSource::SessionEnv => "FRAMESHIFT_SESSION_KEY",
            synapse_core::ResolutionSource::PackPattern => "pack.toml pattern",
            synapse_core::ResolutionSource::Explicit => "explicit/automate",
        };
        eprintln!("{DIM}persona: {} (via {}){RESET}", p.name, src);
    }

    // Name of the persona currently active. Maintained alongside the
    // builder's persona section so per-turn automate re-evaluation can
    // skip the work when the top pick equals the current active.
    let mut active_persona_name: Option<String> = resolved_persona.as_ref().map(|p| p.name.clone());

    // The builder lives outside the bootstrap block so per-turn FSRS
    // recall injection can rebuild only the kleos_recall section
    // without re-resolving persona or re-fetching the skill index.
    let mut prompt_builder = synapse_core::SystemPromptBuilder::with_default_base();
    if is_ollama {
        // Local models stay on the short base; override the base section.
        prompt_builder.set("base", OLLAMA_SYSTEM_PROMPT.to_string());
    }
    prompt_builder.with_persona(resolved_persona.as_ref());

    // Static (foundational) recall captured at session start. Re-merged
    // with per-turn FSRS recall on every turn so the agent never loses
    // the rule-of-thumb infrastructure context.
    let mut startup_recall_blocks: Vec<String> = Vec::new();

    let system_prompt = {
        let kleos = bootstrap_kleos_client().await;

        // Quick health check -- use /health/live (no DB queries, no 1s timeout risk)
        match kleos.get("/health/live").await {
            Ok(_) => eprintln!("{DIM}kleos: connected{RESET}"),
            Err(e) => eprintln!("{DIM}kleos health: {e}{RESET}"),
        }

        // Load Kleos context via /recall (lightweight ranked retrieval).
        // NOTE: /context runs full 8-layer assembly (embeddings + graph + LLM)
        // and exceeds the server's 30s timeout. /recall is what kleos-cli uses.
        {
            let max_retries = 2;
            let mut kleos_ok = false;

            for attempt in 1..=max_retries {
                eprint!("{DIM}Loading Kleos context (attempt {attempt}/{max_retries})...{RESET}");

                let body = serde_json::json!({
                    "query": "agent-rules critical infrastructure",
                    "limit": 30
                });
                match kleos.post("/recall", body).await {
                    Ok(resp) => {
                        if let Some(memories) = resp.get("memories").and_then(|v| v.as_array())
                            && !memories.is_empty()
                        {
                            // Wrap each memory body in <kleos_memory> tags so
                            // the model treats it as untrusted data, not as
                            // a system directive. This closes the prompt-
                            // injection hole the pre-Phase-1 code had where
                            // memory bodies were appended raw.
                            let blocks: Vec<String> = memories
                                .iter()
                                .filter_map(|m| {
                                    let id = m.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let content = m.get("content").and_then(|v| v.as_str())?;
                                    let safe = content.replace('<', "&lt;");
                                    Some(format!(
                                        "<kleos_memory id=\"{id}\">\n{safe}\n</kleos_memory>"
                                    ))
                                })
                                .collect();
                            if !blocks.is_empty() {
                                prompt_builder.with_kleos_recall(&blocks);
                                startup_recall_blocks = blocks;
                            }
                            eprintln!(" {DIM}{} memories{RESET}", memories.len());
                            kleos_ok = true;
                            break;
                        }
                    }
                    Err(e) => eprintln!(" {DIM}{e}{RESET}"),
                }

                if attempt < max_retries {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }

            if !kleos_ok {
                eprintln!("{DIM}kleos: unreachable -- continuing without context{RESET}");
            }
        }

        // Report session start via unified activity endpoint (fans out to
        // Chiasm, Axon, Broca, Thymus, Skills, and Memory automatically).
        {
            let cwd_name = std::env::current_dir()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "unknown".into());

            let summary = if !initial_message.is_empty() {
                initial_message.chars().take(100).collect::<String>()
            } else {
                "interactive REPL session".into()
            };

            let activity_body = serde_json::json!({
                "action": "task.started",
                "summary": summary,
                "project": cwd_name,
                "agent": "synapse",
                "metadata": {
                    "provider": provider_type,
                }
            });

            match kleos.post("/activity", activity_body).await {
                Ok(_) => eprintln!("{DIM}activity: session registered{RESET}"),
                Err(_) => eprintln!("{DIM}activity: unavailable (non-blocking){RESET}"),
            }
        }

        // Inject skill index from Kleos (progressive disclosure).
        // Routed through the builder so the order stays stable across
        // session lifetime and so future /skill reloads can rebuild
        // only this section.
        if !is_ollama {
            eprint!("{DIM}Loading skill index...{RESET}");
            match kleos.get("/skills?limit=30&offset=0").await {
                Ok(body) => {
                    if let Some(skills) = body.get("skills").and_then(|v| v.as_array()) {
                        if !skills.is_empty() {
                            let entries: Vec<synapse_core::SkillIndexEntry> = skills
                                .iter()
                                .map(|s| {
                                    let id = s.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let name =
                                        s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                    let desc =
                                        s.get("description").and_then(|v| v.as_str()).unwrap_or("");
                                    let trust = s
                                        .get("trust_score")
                                        .and_then(|v| v.as_f64())
                                        .unwrap_or(0.0);
                                    let desc_short: String = desc.chars().take(80).collect();
                                    synapse_core::SkillIndexEntry {
                                        name: format!("{name} (#{id}, trust={trust:.1})"),
                                        summary: desc_short,
                                    }
                                })
                                .collect();
                            prompt_builder.with_skill_index(&entries);
                            let skill_count = skills.len();
                            eprintln!(" {DIM}{skill_count} skills{RESET}");
                        } else {
                            eprintln!(" {DIM}none{RESET}");
                        }
                    }
                }
                Err(_) => eprintln!(" {DIM}unavailable{RESET}"),
            }
        }

        prompt_builder.render()
    };

    // Open session store
    let session_store = match SessionStore::open_default() {
        Ok(store) => {
            let count = store.session_count().unwrap_or(0);
            eprintln!("{DIM}Session store: {} sessions{RESET}", count);
            Some(Arc::new(store))
        }
        Err(e) => {
            eprintln!("{DIM}Session store unavailable: {e}{RESET}");
            None
        }
    };

    // Determine project name from cwd
    let project_name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "unknown".into());

    // Load hook configuration from ~/.synapse/hooks.toml if present.
    // Hooks fire at SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop.
    let hooks_config = load_hooks_config();
    // Install a HookGate that wraps PermissiveGate -- in Phase 0 the only
    // gate behavior is hook execution. Phase 3 swaps the inner gate for the
    // interactive TUI permission gate.
    let tool_gate: Option<synapse_tools::SharedGate> = if hooks_config.hooks.is_empty() {
        None
    } else {
        let inner: synapse_tools::SharedGate =
            Arc::new(synapse_tools::PermissiveGate) as synapse_tools::SharedGate;
        Some(Arc::new(synapse_core::HookGate::new(
            Arc::clone(&hooks_config),
            inner,
        )) as synapse_tools::SharedGate)
    };

    let mut config = AgentConfig {
        model: default_model.clone(),
        system_prompt,
        cwd: std::env::current_dir()?,
        max_turns: if is_ollama { 10 } else { 20 },
        max_tokens: if is_ollama { 4096 } else { 8192 },
        session_store: session_store.clone(),
        session_id: None,
        depth: 0,
        compression: if is_ollama {
            None // Local models shouldn't spend tokens on compression
        } else {
            Some(synapse_core::CompressionConfig {
                model: default_model.clone(),
                ..Default::default()
            })
        },
        router: router.clone(),
        max_tool_result_tokens: 4000,
        tool_gate: tool_gate.clone(),
        hooks: Some(Arc::clone(&hooks_config)),
    };

    // SessionStart hooks fire once now, before any user prompt.
    synapse_core::run_phase_hooks(
        &hooks_config,
        synapse_core::HookPhase::SessionStart,
        &config.cwd,
    )
    .await;

    // Register delegate_task tool (needs provider + config, so done after config creation)
    if !is_ollama {
        let spawner_provider = Arc::clone(&provider);
        let spawner_config = config.clone();
        let spawner: Arc<synapse_tools::delegate::AgentSpawner> = Arc::new(
            move |message: String, cwd: std::path::PathBuf, max_turns: usize, child_depth: u8| {
                let provider = Arc::clone(&spawner_provider);
                let parent_config = spawner_config.clone();
                Box::pin(async move {
                    let child_config = AgentConfig {
                        model: parent_config.model.clone(),
                        system_prompt: format!(
                            "{}\n\n## Delegation Context\n\
                             You are a child agent (depth {child_depth}) delegated a specific task. \
                             Complete the task and return a clear summary of what you did and found. \
                             Do NOT use delegate_task, memory-write, or coordination tools. \
                             You have a limited turn budget of {max_turns} turns.",
                            parent_config.system_prompt,
                        ),
                        cwd,
                        max_turns,
                        max_tokens: parent_config.max_tokens,
                        session_store: parent_config.session_store.clone(),
                        session_id: None,
                        depth: child_depth,
                        compression: None,
                        router: None, // Child agents use a single model
                        max_tool_result_tokens: 4000,
                        // Child agents inherit the parent's gate + hooks so
                        // PreToolUse/PostToolUse hooks fire for delegated work too.
                        tool_gate: parent_config.tool_gate.clone(),
                        hooks: parent_config.hooks.clone(),
                    };
                    let child_tools = Arc::new(synapse_tools::delegate::build_child_tools());
                    let stream = agent_loop(child_config, provider, child_tools, message);

                    use futures::StreamExt;
                    let mut stream = Box::pin(stream);
                    let mut text = String::new();
                    let mut input_tokens = 0u32;
                    let mut output_tokens = 0u32;
                    let mut error = None;

                    while let Some(event) = stream.next().await {
                        match event {
                            AgentEvent::Text(t) => text.push_str(&t),
                            AgentEvent::Usage {
                                input_tokens: i,
                                output_tokens: o,
                                ..
                            } => {
                                input_tokens += i;
                                output_tokens += o;
                            }
                            AgentEvent::Error(e) => {
                                error = Some(e);
                                break;
                            }
                            _ => {}
                        }
                    }
                    synapse_tools::delegate::DelegateResult {
                        text,
                        input_tokens,
                        output_tokens,
                        error,
                    }
                })
            },
        );
        tool_registry.register(Box::new(synapse_tools::delegate::DelegateTaskTool::new(
            config.depth,
            AgentConfig::MAX_DEPTH,
            spawner,
        )));
    }

    let tools = Arc::new(tool_registry);

    // Load pricing table for cost telemetry
    let pricing = Arc::new(PricingTable::load());

    // Single-shot mode: message provided on command line
    if !initial_message.is_empty() {
        // Create a session for this single-shot invocation
        if let Some(ref store) = session_store {
            match store.create_session(&project_name, &default_model) {
                Ok(id) => {
                    config.session_id = Some(id);
                    eprintln!("{DIM}Session #{id}{RESET}");
                }
                Err(e) => eprintln!("{DIM}Session create failed: {e}{RESET}"),
            }
        }
        // One-shot mode: fire UserPromptSubmit, run the loop, fire Stop.
        if let Some(ref hc) = config.hooks {
            synapse_core::run_phase_hooks(
                hc,
                synapse_core::HookPhase::UserPromptSubmit,
                &config.cwd,
            )
            .await;
        }
        let stream = Box::pin(synapse_core::agent_loop_with_pricing(
            config.clone(),
            provider,
            tools,
            initial_message,
            Some(Arc::clone(&pricing)),
        ));
        run_event_loop(
            stream,
            config.session_id,
            config.session_store.clone(),
            provider_type,
        )
        .await;
        if let Some(ref hc) = config.hooks {
            synapse_core::run_phase_hooks(hc, synapse_core::HookPhase::Stop, &config.cwd).await;
        }
        return Ok(());
    }

    // REPL mode: no message provided
    {
        let fast_info = if let Some(ref fm) = fast_model {
            format!(" | fast: {fm}")
        } else {
            String::new()
        };
        println!(
            "{BOLD}{CYAN}synapse{RESET} {DIM}v0.1.0 | provider: {provider_type} | model: {default_model}{fast_info} | /quit to exit{RESET}"
        );
    }

    // Mutable state for hot-swappable provider/config
    let mut config = config;
    let mut provider = provider;

    // Create a session for this REPL invocation
    if let Some(ref store) = session_store {
        match store.create_session(&project_name, &default_model) {
            Ok(id) => {
                config.session_id = Some(id);
                eprintln!("{DIM}Session #{id}{RESET}");
            }
            Err(e) => eprintln!("{DIM}Session create failed: {e}{RESET}"),
        }
    }

    let ctx = Arc::new(tokio::sync::Mutex::new(ConversationContext::new(
        config.system_prompt.clone(),
        tools.all_tool_schemas(),
    )));

    // Slash-command registry. Holds the simple, pure commands the
    // CommandRegistry surface fits (quit, persona, model). The
    // stateful commands below (/clear, /tokens, /cost, /settings,
    // /sessions, /resume, /search) stay in the inline match because
    // they need direct access to ctx / session_store / pricing. The
    // builtin /search command is intentionally not registered here --
    // the inline version queries the session DB directly, which is
    // more useful than the registry's default queue-a-prompt behavior.
    let mut command_registry = synapse_core::CommandRegistry::new();
    command_registry.register(Arc::new(synapse_core::QuitCommand));
    command_registry.register(Arc::new(synapse_core::PersonaCommand));
    command_registry.register(Arc::new(synapse_core::ModelCommand));

    loop {
        let input = match read_line(&format!("\n{BOLD}{CYAN}>{RESET} ")) {
            Some(s) if s.is_empty() => continue,
            Some(s) => s,
            None => break,
        };

        // First chance: registry-backed slash commands. If the input
        // starts with a slash and the registry knows the command, the
        // outcome drives the right action. Commands the registry
        // doesn't have fall through to the inline match below.
        if input.starts_with('/')
            && let Some(outcome) = command_registry.dispatch(&input)
        {
            let handled = handle_registry_outcome(
                outcome,
                &mut config,
                &mut prompt_builder,
                &ctx,
                &startup_recall_blocks,
                &mut active_persona_name,
            )
            .await;
            match handled {
                RegistryDispatch::Exit => break,
                RegistryDispatch::Handled => continue,
                RegistryDispatch::Queue(_text) => {
                    // Reserved for `/loop` / future async-prompt
                    // commands. No producer in v1, so for now
                    // the REPL just continues -- the registry
                    // is the right place to add producers later.
                    continue;
                }
            }
        }

        match input.as_str() {
            "/quit" | "/exit" | "/q" => break,
            "/clear" => {
                // Start a fresh session when clearing context
                if let Some(ref store) = session_store {
                    match store.create_session(&project_name, &config.model) {
                        Ok(id) => {
                            config.session_id = Some(id);
                            eprintln!("{DIM}New session #{id}{RESET}");
                        }
                        Err(e) => eprintln!("{DIM}Session create failed: {e}{RESET}"),
                    }
                }
                let mut c = ctx.lock().await;
                *c = ConversationContext::new(
                    config.system_prompt.clone(),
                    tools.all_tool_schemas(),
                );
                println!("{DIM}Context cleared.{RESET}");
                continue;
            }
            "/tokens" => {
                let c = ctx.lock().await;
                let mut info = format!(
                    "~{} tokens, {} messages",
                    c.estimate_tokens(),
                    c.message_count(),
                );
                if let Some(sid) = config.session_id
                    && let Some(ref store) = session_store
                    && let Ok((inp, out)) = store.session_token_counts(sid)
                {
                    info.push_str(&format!(" | session totals: in={inp} out={out}"));
                }
                println!("{DIM}{info}{RESET}");
                continue;
            }
            "/cost" => {
                let c = ctx.lock().await;
                let cost = &c.session_cost;
                println!("\n{BOLD}Session Cost{RESET}");
                println!("{DIM}────────────────────────────────────────{RESET}");
                println!(
                    "  Total: {GREEN}${:.4}{RESET}  ({} in / {} out tokens)",
                    cost.total_usd, cost.total_input_tokens, cost.total_output_tokens
                );
                if cost.total_cache_read_tokens > 0 || cost.total_cache_write_tokens > 0 {
                    println!(
                        "  Cache: {GREEN}{}{RESET} read / {GREEN}{}{RESET} write tokens",
                        cost.total_cache_read_tokens, cost.total_cache_write_tokens
                    );
                }
                if !cost.by_model.is_empty() {
                    println!("\n{BOLD}  By Model{RESET}");
                    for (model, mc) in &cost.by_model {
                        println!(
                            "    {model}: {GREEN}${:.4}{RESET} ({} in / {} out)",
                            mc.usd, mc.input_tokens, mc.output_tokens
                        );
                    }
                }
                // Today's spend: aggregate from the SQLite usage table.
                // Day boundary is local midnight (UTC offset stripped) to match
                // the user's wall clock rather than UTC date.
                if let Some(ref store) = session_store {
                    let now = chrono::Local::now();
                    let midnight = now
                        .date_naive()
                        .and_hms_opt(0, 0, 0)
                        .and_then(|n| n.and_local_timezone(chrono::Local).single())
                        .map(|dt| dt.timestamp())
                        .unwrap_or(0);
                    match store.usage_totals_since(Some(midnight)) {
                        Ok(totals) => {
                            println!(
                                "\n  Today's total: {GREEN}${:.4}{RESET} ({} requests, {} in / {} out)",
                                totals.cost_usd,
                                totals.event_count,
                                totals.input_tokens,
                                totals.output_tokens,
                            );
                        }
                        Err(e) => {
                            eprintln!("{DIM}usage totals unavailable: {e}{RESET}");
                        }
                    }
                }
                println!("{DIM}────────────────────────────────────────{RESET}");
                continue;
            }
            "/settings" => {
                if let Some((new_provider, new_config)) = run_settings_menu(&config) {
                    provider = new_provider;
                    config = new_config;
                    // Reset context with new system prompt
                    let mut c = ctx.lock().await;
                    *c = ConversationContext::new(
                        config.system_prompt.clone(),
                        tools.all_tool_schemas(),
                    );
                    println!("{DIM}Provider swapped. Context cleared.{RESET}");
                }
                continue;
            }
            "/sessions" => {
                if let Some(ref store) = session_store {
                    match store.list_sessions(15, 0) {
                        Ok(sessions) => {
                            if sessions.is_empty() {
                                println!("{DIM}No sessions found.{RESET}");
                            } else {
                                println!("\n{BOLD}  Sessions{RESET}");
                                println!(
                                    "{DIM}  ────────────────────────────────────────────────{RESET}"
                                );
                                for s in &sessions {
                                    let active = if config.session_id == Some(s.id) {
                                        " *"
                                    } else {
                                        ""
                                    };
                                    let summary = s.summary.as_deref().unwrap_or("-");
                                    let summary_short: String = summary.chars().take(50).collect();
                                    let turns = store.turn_count(s.id).unwrap_or(0);
                                    println!(
                                        "  {BOLD}#{}{RESET}{CYAN}{active}{RESET}  {DIM}{} | {} | {turns} turns{RESET}  {}",
                                        s.id, s.project, s.model, summary_short,
                                    );
                                }
                                println!(
                                    "{DIM}  ────────────────────────────────────────────────{RESET}"
                                );
                                println!("{DIM}  /resume <id> to resume a session{RESET}");
                            }
                        }
                        Err(e) => eprintln!("{RED}Failed to list sessions: {e}{RESET}"),
                    }
                } else {
                    println!("{DIM}Session store not available.{RESET}");
                }
                continue;
            }
            _ if input.starts_with("/resume") => {
                let parts: Vec<&str> = input.split_whitespace().collect();
                if parts.len() < 2 {
                    println!("{DIM}Usage: /resume <session_id>{RESET}");
                    continue;
                }
                let id_str = parts[1];
                match id_str.trim_start_matches('#').parse::<i64>() {
                    Ok(target_id) => {
                        if let Some(ref store) = session_store {
                            match store.get_session(target_id) {
                                Ok(Some(session)) => match store.load_messages(target_id) {
                                    Ok(messages) => {
                                        let turn_count = messages.len();
                                        config.session_id = Some(target_id);
                                        let mut c = ctx.lock().await;
                                        *c = ConversationContext::from_history(
                                            config.system_prompt.clone(),
                                            tools.all_tool_schemas(),
                                            messages,
                                        );
                                        println!(
                                            "{DIM}Resumed session #{target_id} ({}, {turn_count} turns){RESET}",
                                            session.project,
                                        );
                                    }
                                    Err(e) => eprintln!("{RED}Failed to load turns: {e}{RESET}"),
                                },
                                Ok(None) => println!("{RED}Session #{target_id} not found.{RESET}"),
                                Err(e) => eprintln!("{RED}Failed to get session: {e}{RESET}"),
                            }
                        } else {
                            println!("{DIM}Session store not available.{RESET}");
                        }
                    }
                    Err(_) => println!("{RED}Invalid session ID: {id_str}{RESET}"),
                }
                continue;
            }
            _ if input.starts_with("/search") => {
                let query = input.strip_prefix("/search").unwrap_or("").trim();
                if query.is_empty() {
                    println!("{DIM}Usage: /search <query>{RESET}");
                    continue;
                }
                if let Some(ref store) = session_store {
                    match store.search(query, 10) {
                        Ok(results) => {
                            if results.is_empty() {
                                println!("{DIM}No results for \"{query}\".{RESET}");
                            } else {
                                println!("\n{BOLD}  Search: {query}{RESET}");
                                println!(
                                    "{DIM}  ────────────────────────────────────────────────{RESET}"
                                );
                                for r in &results {
                                    println!(
                                        "  {DIM}session #{} | {}{RESET} ({CYAN}{}{RESET})",
                                        r.session_id, r.project, r.role,
                                    );
                                    // Clean up FTS5 markers for display
                                    let snippet = r
                                        .snippet
                                        .replace(">>>", BOLD)
                                        .replace("<<<", &format!("{RESET}{DIM}"));
                                    println!("  {DIM}  {snippet}{RESET}");
                                }
                                println!(
                                    "{DIM}  ────────────────────────────────────────────────{RESET}"
                                );
                            }
                        }
                        Err(e) => eprintln!("{RED}Search failed: {e}{RESET}"),
                    }
                } else {
                    println!("{DIM}Session store not available.{RESET}");
                }
                continue;
            }
            _ => {}
        }

        // Fire UserPromptSubmit hooks before sending the message to the LLM.
        // Hooks run in the configured cwd; failures are logged but don't block.
        if let Some(ref hc) = config.hooks {
            synapse_core::run_phase_hooks(
                hc,
                synapse_core::HookPhase::UserPromptSubmit,
                &config.cwd,
            )
            .await;
        }

        // Automate per-turn re-evaluation. When the Frameshift engine
        // has automate mode on for this project, ask the ranker which
        // persona fits the new prompt and swap if the domain has
        // shifted enough to justify the context cost.
        if automate.on && !is_ollama {
            let task_summary: String = input.chars().take(280).collect();
            match synapse_core::select_for_task(&task_summary, &automate_opts) {
                Ok(Some(pick)) if Some(pick.name.as_str()) != active_persona_name.as_deref() => {
                    match synapse_core::load_by_name(&pick.name, &persona_opts) {
                        Ok(Some(new_p)) => {
                            prompt_builder.with_persona(Some(&new_p));
                            if !startup_recall_blocks.is_empty() {
                                prompt_builder.with_kleos_recall(&startup_recall_blocks);
                            }
                            let rendered = prompt_builder.render();
                            {
                                let mut c = ctx.lock().await;
                                c.set_system(rendered.clone());
                            }
                            config.system_prompt = rendered;
                            eprintln!(
                                "{DIM}automate: swap -> {} (score {:.2}, margin {:.2}){RESET}",
                                new_p.name, pick.score, pick.margin
                            );
                            active_persona_name = Some(new_p.name);
                        }
                        Ok(None) => log::debug!(
                            "automate: top pick {} missing from disk, holding",
                            pick.name
                        ),
                        Err(e) => log::debug!("automate: load failed: {e}"),
                    }
                }
                Ok(_) => {
                    // Pick equals current persona (hold) or no confident
                    // pick (also hold). Quiet path -- automate decisions
                    // shouldn't spam the user when nothing changes.
                }
                Err(e) => log::debug!("automate: select failed: {e}"),
            }
        }

        // FSRS-backed recall: surface memories due for reinforcement on
        // the user's current topic, merged with the static startup
        // recall. Failures are silent -- recall is opportunistic and
        // must never block the turn.
        if !is_ollama {
            let topic: String = input.chars().take(280).collect();
            let recall_opts = synapse_tools::RecallOptions {
                topic,
                limit: 5,
                retrievability_max: 0.7,
                session: Some(project_name.clone()),
            };
            let fsrs_blocks = synapse_tools::recall_due_as_blocks(&recall_opts).await;
            if !fsrs_blocks.is_empty() {
                let mut merged = startup_recall_blocks.clone();
                merged.extend(fsrs_blocks);
                prompt_builder.with_kleos_recall(&merged);
                let new_prompt = prompt_builder.render();
                {
                    let mut c = ctx.lock().await;
                    c.set_system(new_prompt.clone());
                }
                // Also update the cloned-per-turn config so models that
                // read system from AgentConfig (not ConversationContext)
                // see the same string.
                config.system_prompt = new_prompt;
            }
        }

        let stream = Box::pin(agent_turn_with_pricing(
            config.clone(),
            Arc::clone(&provider),
            Arc::clone(&tools),
            Arc::clone(&ctx),
            input,
            Some(Arc::clone(&pricing)),
        ));
        run_event_loop(
            stream,
            config.session_id,
            config.session_store.clone(),
            provider_type,
        )
        .await;
    }

    // Stop hooks fire once when the REPL exits.
    if let Some(ref hc) = config.hooks {
        synapse_core::run_phase_hooks(hc, synapse_core::HookPhase::Stop, &config.cwd).await;
    }

    println!("{DIM}bye{RESET}");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Interactive settings menu
// ─────────────────────────────────────────────────────────────────────────────

/// Load the synapse config.json, or return an empty object.
fn load_settings() -> serde_json::Value {
    let path = dirs::home_dir()
        .map(|h| h.join(".synapse").join("config.json"))
        .unwrap_or_default();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Save the config back to ~/.synapse/config.json.
fn save_settings(config: &serde_json::Value) {
    let path = dirs::home_dir()
        .map(|h| h.join(".synapse").join("config.json"))
        .unwrap_or_default();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(config) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("{RED}Failed to save config: {e}{RESET}");
            } else {
                println!("{DIM}Config saved.{RESET}");
            }
        }
        Err(e) => eprintln!("{RED}Failed to serialize config: {e}{RESET}"),
    }
}

/// Mask a secret string for display.
fn mask(s: &str) -> String {
    if s.is_empty() {
        "(not set)".into()
    } else if s.len() <= 8 {
        "****".into()
    } else {
        format!("{}****", &s[..4])
    }
}

/// Interactive settings menu. Returns new provider + config if changed, None if cancelled.
fn run_settings_menu(
    current_config: &AgentConfig,
) -> Option<(
    Arc<dyn synapse_provider::Provider + Send + Sync>,
    AgentConfig,
)> {
    let mut settings = load_settings();

    loop {
        let provider = settings
            .get("provider")
            .and_then(|v| v.as_str())
            .map(canonical_provider_name)
            .unwrap_or("proxy")
            .to_owned();
        let model = settings
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-20250514")
            .to_owned();
        let proxy_url = settings
            .get("openai_base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let proxy_key = settings
            .get("openai_api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let ollama_url = settings
            .get("ollama_url")
            .and_then(|v| v.as_str())
            .unwrap_or("http://localhost:11434/v1")
            .to_owned();
        let kleos_url = settings
            .get("kleos_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let kleos_key = settings
            .get("kleos_api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let max_tokens = settings
            .get("max_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(8192);
        let zen_key = settings
            .get("opencode_zen_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let zen_url = settings
            .get("opencode_zen_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let azure_endpoint = settings
            .get("azure_endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let azure_deployment = settings
            .get("azure_deployment")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let azure_key = settings
            .get("azure_api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let foundry_host = settings
            .get("foundry_host")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let foundry_token = settings
            .get("foundry_token")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();

        println!("\n{BOLD}{CYAN}  Settings{RESET}");
        println!("{DIM}  ─────────────────────────────────────{RESET}");
        println!("  {BOLD}1.{RESET} Provider       {CYAN}{provider}{RESET}");
        println!("  {BOLD}2.{RESET} Model          {CYAN}{model}{RESET}");
        println!(
            "  {BOLD}3.{RESET} Proxy URL      {DIM}{}{RESET}",
            if proxy_url.is_empty() {
                "(not set)"
            } else {
                &proxy_url
            }
        );
        println!(
            "  {BOLD}4.{RESET} Proxy Key      {DIM}{}{RESET}",
            mask(&proxy_key)
        );
        println!("  {BOLD}5.{RESET} Ollama URL     {DIM}{ollama_url}{RESET}");
        println!(
            "  {BOLD}6.{RESET} Kleos URL      {DIM}{}{RESET}",
            if kleos_url.is_empty() {
                "(not set -- default localhost:4200)"
            } else {
                &kleos_url
            }
        );
        println!(
            "  {BOLD}7.{RESET} Kleos Key      {DIM}{}{RESET}",
            mask(&kleos_key)
        );
        println!("  {BOLD}8.{RESET} Max Tokens     {DIM}{max_tokens}{RESET}");
        println!(
            "  {BOLD}9.{RESET} Zen Key        {DIM}{}{RESET}",
            mask(&zen_key)
        );
        println!(
            " {BOLD}10.{RESET} Zen URL        {DIM}{}{RESET}",
            if zen_url.is_empty() {
                "(default: opencode.ai/zen/v1)"
            } else {
                &zen_url
            }
        );
        println!(
            " {BOLD}11.{RESET} Azure Endpoint {DIM}{}{RESET}",
            if azure_endpoint.is_empty() {
                "(not set)"
            } else {
                &azure_endpoint
            }
        );
        println!(
            " {BOLD}12.{RESET} Azure Deploy   {DIM}{}{RESET}",
            if azure_deployment.is_empty() {
                "(not set)"
            } else {
                &azure_deployment
            }
        );
        println!(
            " {BOLD}13.{RESET} Azure Key      {DIM}{}{RESET}",
            mask(&azure_key)
        );
        println!(
            " {BOLD}14.{RESET} Foundry Host   {DIM}{}{RESET}",
            if foundry_host.is_empty() {
                "(not set)"
            } else {
                &foundry_host
            }
        );
        println!(
            " {BOLD}15.{RESET} Foundry Token  {DIM}{}{RESET}",
            mask(&foundry_token)
        );
        println!("{DIM}  ─────────────────────────────────────{RESET}");
        println!(
            "  {DIM}Enter number to edit, {BOLD}s{RESET}{DIM} to save & apply, {BOLD}q{RESET}{DIM} to cancel{RESET}"
        );

        let choice = read_line(&format!("  {CYAN}#{RESET} "))?;

        match choice.as_str() {
            "q" | "" => return None,
            "s" => {
                save_settings(&settings);
                // Rebuild provider from updated settings
                let prov = settings
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .map(canonical_provider_name)
                    .unwrap_or("proxy");
                let mdl = settings
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("claude-sonnet-4-20250514")
                    .to_owned();
                let is_local = prov == "ollama";

                let provider_config = match prov {
                    "openai-codex" => {
                        let base_url = settings
                            .get("openai_codex_url")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_owned());
                        ProviderConfig::OpenAICodexAuto {
                            auth_path: None,
                            base_url,
                        }
                    }
                    "ollama" => {
                        let url = settings
                            .get("ollama_url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        ProviderConfig::Ollama {
                            base_url: if url.is_empty() || url == "http://localhost:11434/v1" {
                                None
                            } else {
                                Some(url.to_owned())
                            },
                        }
                    }
                    "anthropic" => ProviderConfig::AnthropicAuto,
                    "opencode-zen" | "zen" | "opencode" => {
                        let key = settings
                            .get("opencode_zen_key")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        if key.is_empty() {
                            eprintln!("{RED}OpenCode Zen requires an API key (option 9).{RESET}");
                            continue;
                        }
                        let base_url = settings
                            .get("opencode_zen_url")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_owned());
                        ProviderConfig::OpenCodeZen {
                            api_key: key,
                            base_url,
                        }
                    }
                    "azure" => {
                        let endpoint = settings
                            .get("azure_endpoint")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        let deployment = settings
                            .get("azure_deployment")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        let key = settings
                            .get("azure_api_key")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        if endpoint.is_empty() || deployment.is_empty() || key.is_empty() {
                            eprintln!(
                                "{RED}Azure requires endpoint (11), deployment (12), key (13).{RESET}"
                            );
                            continue;
                        }
                        let api_version = settings
                            .get("azure_api_version")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_owned());
                        ProviderConfig::Azure {
                            endpoint,
                            deployment,
                            api_key: key,
                            api_version,
                        }
                    }
                    "foundry-openai" => {
                        let host = settings
                            .get("foundry_host")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        let token = settings
                            .get("foundry_token")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        if host.is_empty() || token.is_empty() {
                            eprintln!("{RED}Foundry requires host (14) and token (15).{RESET}");
                            continue;
                        }
                        ProviderConfig::FoundryOpenAI { host, token }
                    }
                    "foundry-anthropic" => {
                        let host = settings
                            .get("foundry_host")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        let token = settings
                            .get("foundry_token")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        if host.is_empty() || token.is_empty() {
                            eprintln!("{RED}Foundry requires host (14) and token (15).{RESET}");
                            continue;
                        }
                        ProviderConfig::FoundryAnthropic { host, token }
                    }
                    _ => {
                        let base = settings
                            .get("openai_base_url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        let key = settings
                            .get("openai_api_key")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_owned();
                        if key.is_empty() {
                            eprintln!("{RED}Proxy requires an API key.{RESET}");
                            continue;
                        }
                        ProviderConfig::Proxy {
                            base_url: base,
                            api_key: key,
                        }
                    }
                };

                match create_provider(provider_config) {
                    Ok(new_provider) => {
                        let system_prompt = if is_local {
                            OLLAMA_SYSTEM_PROMPT.to_string()
                        } else {
                            current_config.system_prompt.clone()
                        };
                        let max_tok = settings
                            .get("max_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(if is_local { 4096 } else { 8192 })
                            as u32;

                        let new_config = AgentConfig {
                            model: mdl.clone(),
                            system_prompt,
                            cwd: current_config.cwd.clone(),
                            max_turns: if is_local { 10 } else { 20 },
                            max_tokens: max_tok,
                            session_store: current_config.session_store.clone(),
                            session_id: current_config.session_id,
                            depth: current_config.depth,
                            compression: if is_local {
                                None
                            } else {
                                Some(synapse_core::CompressionConfig {
                                    model: mdl,
                                    ..Default::default()
                                })
                            },
                            router: None, // Settings swap resets routing
                            max_tool_result_tokens: 4000,
                            // Preserve gate and hooks across settings reload
                            tool_gate: current_config.tool_gate.clone(),
                            hooks: current_config.hooks.clone(),
                        };

                        let wrapped: Arc<dyn synapse_provider::Provider + Send + Sync> =
                            Arc::new(ProviderWrapper(new_provider));
                        println!("\n  {CYAN}{BOLD}{prov}{RESET} {DIM}active{RESET}");
                        return Some((wrapped, new_config));
                    }
                    Err(e) => {
                        eprintln!("{RED}Failed to create provider: {e}{RESET}");
                        continue;
                    }
                }
            }
            "1" => {
                println!(
                    "  {DIM}Options: openai-codex, opencode-zen, anthropic, proxy, ollama, azure, foundry-openai, foundry-anthropic{RESET}"
                );
                if let Some(val) = read_line(&format!("  {CYAN}provider>{RESET} "))
                    && !val.is_empty()
                {
                    settings["provider"] = serde_json::Value::String(val.clone());
                    // Auto-set model defaults when switching provider
                    if val == "openai-codex" || val == "codex" {
                        settings["provider"] = serde_json::Value::String("openai-codex".into());
                        let m = settings.get("model").and_then(|v| v.as_str()).unwrap_or("");
                        if should_reset_openai_codex_model(m) {
                            settings["model"] = serde_json::Value::String(
                                synapse_provider::openai_codex::DEFAULT_MODEL.into(),
                            );
                        }
                    } else if val == "ollama"
                        && (model.starts_with("claude") || model.starts_with("gpt"))
                    {
                        settings["model"] = serde_json::Value::String("qwen2.5:7b".into());
                        println!("  {DIM}Model auto-set to qwen2.5:7b{RESET}");
                    } else if val == "foundry-openai" {
                        let m = settings.get("model").and_then(|v| v.as_str()).unwrap_or("");
                        if m.is_empty() || !m.starts_with("ri.language-model-service") {
                            settings["model"] = serde_json::Value::String(
                                "ri.language-model-service..language-model.gpt-4-1".into(),
                            );
                            println!("  {DIM}Model auto-set to gpt-4-1{RESET}");
                        }
                    } else if val == "foundry-anthropic" {
                        let m = settings.get("model").and_then(|v| v.as_str()).unwrap_or("");
                        if m.is_empty() || !m.starts_with("ri.language-model-service") {
                            settings["model"] = serde_json::Value::String(
                                "ri.language-model-service..language-model.anthropic-claude-4-6-sonnet".into(),
                            );
                            println!("  {DIM}Model auto-set to claude-4-6-sonnet{RESET}");
                        }
                    } else if val == "opencode-zen" || val == "zen" || val == "opencode" {
                        settings["provider"] = serde_json::Value::String("opencode-zen".into());
                        // Auto-set a sensible default if model is missing or for a different provider
                        let m = settings.get("model").and_then(|v| v.as_str()).unwrap_or("");
                        if m.is_empty() || m.starts_with("qwen2") {
                            settings["model"] = serde_json::Value::String(
                                synapse_provider::opencode_zen::DEFAULT_MODEL.into(),
                            );
                        }
                        println!(
                            "  {DIM}Presets: {}{RESET}",
                            synapse_provider::opencode_zen::MODEL_PRESETS.join(", ")
                        );
                    }
                }
            }
            "2" => {
                if let Some(val) = read_line(&format!("  {CYAN}model>{RESET} "))
                    && !val.is_empty()
                {
                    settings["model"] = serde_json::Value::String(val);
                }
            }
            "3" => {
                if let Some(val) = read_line(&format!("  {CYAN}proxy url>{RESET} "))
                    && !val.is_empty()
                {
                    settings["openai_base_url"] = serde_json::Value::String(val);
                }
            }
            "4" => {
                if let Some(val) = read_line(&format!("  {CYAN}proxy key>{RESET} "))
                    && !val.is_empty()
                {
                    settings["openai_api_key"] = serde_json::Value::String(val);
                }
            }
            "5" => {
                println!("  {DIM}Default: http://localhost:11434/v1{RESET}");
                if let Some(val) = read_line(&format!("  {CYAN}ollama url>{RESET} "))
                    && !val.is_empty()
                {
                    settings["ollama_url"] = serde_json::Value::String(val);
                }
            }
            "6" => {
                if let Some(val) = read_line(&format!("  {CYAN}kleos url>{RESET} "))
                    && !val.is_empty()
                {
                    settings["kleos_url"] = serde_json::Value::String(val);
                }
            }
            "7" => {
                if let Some(val) = read_line(&format!("  {CYAN}kleos key>{RESET} "))
                    && !val.is_empty()
                {
                    settings["kleos_api_key"] = serde_json::Value::String(val);
                }
            }
            "8" => {
                if let Some(val) = read_line(&format!("  {CYAN}max tokens>{RESET} ")) {
                    if let Ok(n) = val.parse::<u64>() {
                        settings["max_tokens"] = serde_json::Value::Number(n.into());
                    } else {
                        eprintln!("  {RED}Invalid number{RESET}");
                    }
                }
            }
            "9" => {
                if let Some(val) = read_line(&format!("  {CYAN}zen key>{RESET} "))
                    && !val.is_empty()
                {
                    settings["opencode_zen_key"] = serde_json::Value::String(val);
                }
            }
            "10" => {
                println!(
                    "  {DIM}Default: {}{RESET}",
                    synapse_provider::opencode_zen::DEFAULT_BASE_URL
                );
                if let Some(val) = read_line(&format!("  {CYAN}zen url>{RESET} "))
                    && !val.is_empty()
                {
                    settings["opencode_zen_url"] = serde_json::Value::String(val);
                }
            }
            "11" => {
                if let Some(val) = read_line(&format!("  {CYAN}azure endpoint>{RESET} "))
                    && !val.is_empty()
                {
                    settings["azure_endpoint"] = serde_json::Value::String(val);
                }
            }
            "12" => {
                if let Some(val) = read_line(&format!("  {CYAN}azure deployment>{RESET} "))
                    && !val.is_empty()
                {
                    settings["azure_deployment"] = serde_json::Value::String(val);
                }
            }
            "13" => {
                if let Some(val) = read_line(&format!("  {CYAN}azure key>{RESET} "))
                    && !val.is_empty()
                {
                    settings["azure_api_key"] = serde_json::Value::String(val);
                }
            }
            "14" => {
                println!("  {DIM}e.g. yourstack.usw-22.palantirfoundry.com{RESET}");
                if let Some(val) = read_line(&format!("  {CYAN}foundry host>{RESET} "))
                    && !val.is_empty()
                {
                    settings["foundry_host"] = serde_json::Value::String(val);
                }
            }
            "15" => {
                println!(
                    "  {DIM}Generate at Foundry > Settings > Tokens (scope: api:usage:language-models-execute){RESET}"
                );
                if let Some(val) = read_line(&format!("  {CYAN}foundry token>{RESET} "))
                    && !val.is_empty()
                {
                    settings["foundry_token"] = serde_json::Value::String(val);
                }
            }
            _ => println!("  {DIM}Invalid option{RESET}"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// `synapse doctor` -- diagnose provider config and connectivity
// ─────────────────────────────────────────────────────────────────────────────

// YELLOW lives with the other ANSI colors at the top of this module.

/// Returns process exit code: 0 if all checks pass, 1 if any failed.
async fn run_doctor(override_provider: Option<&str>) -> i32 {
    println!("{BOLD}{CYAN}synapse doctor{RESET}");
    println!("{DIM}─────────────────────────────────────{RESET}");

    let settings = load_settings();
    let mut failed = false;

    // 1. Resolve which provider is configured
    let provider = override_provider
        .map(canonical_provider_name)
        .map(|s| s.to_string())
        .or_else(|| {
            settings
                .get("provider")
                .and_then(|v| v.as_str())
                .map(canonical_provider_name)
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| autodetect_default_provider().to_string());
    println!("  provider:    {CYAN}{provider}{RESET}");

    // 2. Provider-specific connectivity probe
    match provider.as_str() {
        "openai-codex" | "codex" => {
            let auth = synapse_provider::openai_codex::CodexAuth::from_path(
                synapse_provider::openai_codex::CodexAuth::default_path(),
            );
            match auth.status() {
                Ok(synapse_provider::openai_codex::AuthStatus::Ready { expires_at }) => {
                    println!("  {GREEN}✓ login present{RESET}");
                    println!("  expires at:   {DIM}{expires_at}{RESET}");
                    match synapse_provider::openai_codex::load_auth_file(auth.path())
                        .and_then(|file| file.openai_codex())
                    {
                        Ok(Some(entry)) => {
                            let base_url =
                                config_key("SYNAPSE_OPENAI_CODEX_URL", "openai_codex_url");
                            let base_url = if base_url.is_empty() {
                                entry.base_url
                            } else {
                                base_url
                            };
                            let configured_model = config_key("SYNAPSE_MODEL", "model");
                            let doctor_model = doctor_openai_codex_model(
                                (!configured_model.is_empty()).then_some(configured_model.as_str()),
                            );
                            println!("  base url:    {DIM}{base_url}{RESET}");
                            println!("  model:       {DIM}{doctor_model}{RESET}");
                            failed |= !probe_responses(
                                &base_url,
                                &entry.tokens.access_token,
                                &doctor_model,
                            )
                            .await;
                        }
                        Ok(None) => {
                            println!("  {RED}✗ auth entry missing{RESET}");
                            failed = true;
                        }
                        Err(e) => {
                            println!("  {RED}✗ auth error: {e}{RESET}");
                            failed = true;
                        }
                    }
                }
                Ok(synapse_provider::openai_codex::AuthStatus::RefreshNeeded) => {
                    println!("  {RED}✗ token expired{RESET} -- run `synapse login openai-codex`");
                    failed = true;
                }
                Ok(synapse_provider::openai_codex::AuthStatus::Missing) => {
                    println!("  {RED}✗ not logged in{RESET} -- run `synapse login openai-codex`");
                    failed = true;
                }
                Err(e) => {
                    println!("  {RED}✗ auth error: {e}{RESET}");
                    failed = true;
                }
            }
        }
        "opencode-zen" | "zen" | "opencode" => {
            let explicit_key = config_key("SYNAPSE_OPENCODE_KEY", "opencode_zen_key");
            let url_override = config_key("SYNAPSE_OPENCODE_URL", "opencode_zen_url");
            let base_url = if url_override.is_empty() {
                synapse_provider::opencode_zen::DEFAULT_BASE_URL.to_string()
            } else {
                url_override
            };
            println!("  base url:    {DIM}{base_url}{RESET}");
            let (key, source) = if !explicit_key.is_empty() {
                (explicit_key, "env/config")
            } else if let Some(tok) = synapse_provider::opencode_zen::load_subscription_token() {
                (tok, "auth.json (opencode-go)")
            } else {
                (String::new(), "")
            };
            if key.is_empty() {
                println!(
                    "  {RED}✗ no key found{RESET} -- run `opencode providers` to log in, or set SYNAPSE_OPENCODE_KEY"
                );
                failed = true;
            } else {
                println!("  api key:     {DIM}{} (from {source}){RESET}", mask(&key));
                failed |= !probe_models(&base_url, Some(&key), false).await;
                // Real auth/billing check: tiny chat round-trip with max_tokens=1.
                failed |= !probe_chat(&base_url, &key).await;
            }
        }
        "proxy" => {
            let key = config_key("SYNAPSE_PROXY_KEY", "openai_api_key");
            let url = config_key("SYNAPSE_PROXY_URL", "openai_base_url");
            if url.is_empty() {
                println!("  {RED}✗ SYNAPSE_PROXY_URL not configured{RESET}");
                failed = true;
            } else {
                println!("  base url:    {DIM}{url}{RESET}");
                println!("  api key:     {DIM}{}{RESET}", mask(&key));
                if key.is_empty() {
                    println!("  {RED}✗ no api key set{RESET}");
                    failed = true;
                } else {
                    failed |= !probe_models(&url, Some(&key), false).await;
                }
            }
        }
        "azure" => {
            let endpoint = config_key("SYNAPSE_AZURE_ENDPOINT", "azure_endpoint");
            let deployment = config_key("SYNAPSE_AZURE_DEPLOYMENT", "azure_deployment");
            let key = config_key("SYNAPSE_AZURE_KEY", "azure_api_key");
            println!("  endpoint:    {DIM}{endpoint}{RESET}");
            println!("  deployment:  {DIM}{deployment}{RESET}");
            println!("  api key:     {DIM}{}{RESET}", mask(&key));
            if endpoint.is_empty() || deployment.is_empty() || key.is_empty() {
                println!("  {RED}✗ azure config incomplete{RESET}");
                failed = true;
            } else {
                let url = format!("{}/openai/deployments", endpoint.trim_end_matches('/'));
                failed |= !probe_url(&url, Some(("api-key", &key))).await;
            }
        }
        "ollama" => {
            let url = {
                let v = config_key("SYNAPSE_OLLAMA_URL", "ollama_url");
                if v.is_empty() {
                    "http://localhost:11434/v1".into()
                } else {
                    v
                }
            };
            println!("  base url:    {DIM}{url}{RESET}");
            failed |= !probe_models(&url, None, true).await;
        }
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
            println!("  api key:     {DIM}{}{RESET}", mask(&key));
            if key.is_empty() {
                println!(
                    "  {YELLOW}? no ANTHROPIC_API_KEY in env -- will try OpenCode auth.json{RESET}"
                );
            } else {
                println!("  {GREEN}✓ key present (no live probe){RESET}");
            }
        }
        other => {
            println!("  {RED}✗ unknown provider: {other}{RESET}");
            failed = true;
        }
    }

    // 3. Kleos reachability (informational, not fatal)
    println!("{DIM}─────────────────────────────────────{RESET}");
    let kleos = bootstrap_kleos_client().await;
    let kleos_url =
        std::env::var("KLEOS_URL").unwrap_or_else(|_| "http://localhost:4200".to_string());
    println!("  kleos url:   {DIM}{kleos_url}{RESET}");
    match kleos.get("/health").await {
        Ok(_) => {
            println!("  {GREEN}✓ kleos reachable{RESET}");
        }
        Err(e) => {
            println!("  {YELLOW}? kleos unreachable: {e}{RESET}");
        }
    }

    println!("{DIM}─────────────────────────────────────{RESET}");
    if failed {
        println!("{RED}doctor: failed{RESET}");
        1
    } else {
        println!("{GREEN}doctor: ok{RESET}");
        0
    }
}

/// Probe `{base}/models` and report number returned. Returns true on success.
async fn probe_models(base_url: &str, bearer: Option<&str>, is_ollama: bool) -> bool {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let mut req = client.get(&url);
    if let Some(key) = bearer {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                let snippet: String = body.chars().take(120).collect();
                println!("  {RED}✗ {status} on {url}{RESET}");
                if !snippet.is_empty() {
                    println!("    {DIM}{snippet}{RESET}");
                }
                return false;
            }
            match resp.json::<serde_json::Value>().await {
                Ok(body) => {
                    let count = body
                        .get("data")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .or_else(|| {
                            body.get("models")
                                .and_then(|v| v.as_array())
                                .map(|a| a.len())
                        })
                        .unwrap_or(0);
                    if count == 0 && is_ollama {
                        println!("  {YELLOW}? ollama reachable but no models pulled{RESET}");
                        return true;
                    }
                    println!("  {GREEN}✓ {count} models available{RESET}");
                    true
                }
                Err(e) => {
                    println!("  {YELLOW}? response not JSON: {e}{RESET}");
                    true
                }
            }
        }
        Err(e) => {
            println!("  {RED}✗ network error: {e}{RESET}");
            false
        }
    }
}

/// Real auth/billing probe: send a 1-token chat completion and surface upstream
/// error bodies (e.g. CreditsError, invalid model). Negligible cost; catches the
/// failure modes that `/v1/models` doesn't (which is often unauthenticated).
async fn probe_chat(base_url: &str, key: &str) -> bool {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": synapse_provider::opencode_zen::DEFAULT_MODEL,
        "messages": [{"role": "user", "content": "."}],
        "max_tokens": 1,
        "stream": false,
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap();
    match client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                println!("  {GREEN}✓ chat round-trip ok ({status}){RESET}");
                true
            } else {
                // Try to surface a structured upstream error message.
                let msg = serde_json::from_str::<serde_json::Value>(&body_text)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| body_text.chars().take(200).collect());
                println!("  {RED}✗ chat failed ({status}){RESET}");
                println!("    {DIM}{msg}{RESET}");
                false
            }
        }
        Err(e) => {
            println!("  {RED}✗ chat network error: {e}{RESET}");
            false
        }
    }
}

/// Send a tiny OpenAI Responses request to verify Codex auth and endpoint reachability.
async fn probe_responses(base_url: &str, key: &str, model: &str) -> bool {
    let url = synapse_provider::openai_codex::response_endpoint(base_url);
    let body = serde_json::json!({
        "model": model,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "."}]}],
        "max_output_tokens": 1,
        "stream": false,
        "store": false,
    });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap();
    match client
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status();
            let body_text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                println!("  {GREEN}✓ responses round-trip ok ({status}){RESET}");
                true
            } else {
                let snippet: String = body_text.chars().take(200).collect();
                println!("  {RED}✗ responses probe failed ({status}){RESET}");
                if !snippet.is_empty() {
                    println!("    {DIM}{snippet}{RESET}");
                }
                false
            }
        }
        Err(e) => {
            println!("  {RED}✗ responses network error: {e}{RESET}");
            false
        }
    }
}

/// Generic GET probe -- success on any 2xx/4xx (4xx still proves the endpoint exists).
async fn probe_url(url: &str, header: Option<(&str, &str)>) -> bool {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let mut req = client.get(url);
    if let Some((k, v)) = header {
        req = req.header(k, v);
    }
    match req.send().await {
        Ok(resp) => {
            let s = resp.status();
            if s.is_success() {
                println!("  {GREEN}✓ endpoint reachable ({s}){RESET}");
                true
            } else if s.as_u16() == 401 || s.as_u16() == 403 {
                println!("  {RED}✗ auth failed ({s}){RESET}");
                false
            } else {
                println!("  {YELLOW}? {s} on {url}{RESET}");
                true
            }
        }
        Err(e) => {
            println!("  {RED}✗ network error: {e}{RESET}");
            false
        }
    }
}

/// Assert the callback query helper URL-decodes values before returning them.
#[cfg(test)]
fn assert_query_value_decodes_urlencoded_values() {
    let path = "/auth/callback?code=abc123&error_description=Needs%20login";
    assert_eq!(query_value(path, "code").as_deref(), Some("abc123"));
    assert_eq!(
        query_value(path, "error_description").as_deref(),
        Some("Needs login")
    );
}

/// Keep the focused cargo test filter stable for the Task 4 verification command.
#[cfg(test)]
#[test]
fn query_value_decodes_urlencoded_values() {
    assert_query_value_decodes_urlencoded_values();
}

/// Assert that the stable `codex` alias resolves onto the canonical provider id.
#[cfg(test)]
fn assert_openai_codex_provider_aliases_normalize() {
    assert_eq!(canonical_provider_name("codex"), "openai-codex");
    assert_eq!(canonical_provider_name("openai-codex"), "openai-codex");
}

/// Keep the focused Task 5 verification command bound to a root test name.
#[cfg(test)]
#[test]
fn openai_codex_provider_aliases_normalize() {
    assert_openai_codex_provider_aliases_normalize();
}

/// Covers the small CLI-only OpenAI Codex auth helpers.
#[cfg(test)]
mod openai_codex_cli_tests {
    use super::*;

    /// Ensures canonical provider normalization maps the Codex alias onto the stored id.
    #[test]
    fn canonical_provider_name_normalizes_codex_alias() {
        assert_openai_codex_provider_aliases_normalize();
    }

    /// Ensures provider autodetect still falls back to Anthropic after Codex and Zen.
    #[test]
    fn autodetected_provider_preserves_anthropic_fallback() {
        assert_eq!(autodetected_provider(true, true, true), "openai-codex");
        assert_eq!(autodetected_provider(false, true, true), "opencode-zen");
        assert_eq!(autodetected_provider(false, false, true), "anthropic");
        assert_eq!(autodetected_provider(false, false, false), "proxy");
    }

    /// Ensures switching to OpenAI Codex resets models that obviously belong to other providers.
    #[test]
    fn should_reset_openai_codex_model_rejects_other_provider_models() {
        for model in [
            "",
            "claude-sonnet-4-20250514",
            "qwen2.5:7b",
            "kimi-k2.5",
            "ri.language-model-service..language-model.gpt-4-1",
        ] {
            assert!(
                should_reset_openai_codex_model(model),
                "expected `{model}` to be replaced"
            );
        }
    }

    /// Ensures switching to OpenAI Codex preserves models that are already plausible there.
    #[test]
    fn should_reset_openai_codex_model_preserves_codex_friendly_models() {
        for model in ["codex-mini-latest", "codex-max-latest", "gpt-5"] {
            assert!(
                !should_reset_openai_codex_model(model),
                "expected `{model}` to be preserved"
            );
        }
    }

    /// Ensures the doctor probe uses the configured Codex model when one is set.
    #[test]
    fn doctor_openai_codex_model_prefers_configured_model() {
        assert_eq!(
            doctor_openai_codex_model(Some("codex-max-latest")),
            "codex-max-latest"
        );
        assert_eq!(
            doctor_openai_codex_model(Some("")),
            synapse_provider::openai_codex::DEFAULT_MODEL
        );
        assert_eq!(
            doctor_openai_codex_model(None),
            synapse_provider::openai_codex::DEFAULT_MODEL
        );
    }

    /// Ensures callback query parsing URL-decodes values before returning them.
    #[test]
    fn query_value_decodes_urlencoded_values() {
        assert_query_value_decodes_urlencoded_values();
    }

    /// Ensures callback query parsing treats `+` as a space in query values.
    #[test]
    fn query_value_decodes_plus_as_space() {
        let path = "/auth/callback?error_description=Needs+login+now";
        assert_eq!(
            query_value(path, "error_description").as_deref(),
            Some("Needs login now")
        );
    }

    /// Ensures callback parsing returns the authorization code on a valid callback.
    #[test]
    fn parse_openai_codex_callback_accepts_matching_state_and_code() {
        let path = "/auth/callback?state=expected-state&code=abc123";
        assert_eq!(
            parse_openai_codex_callback(path, "expected-state").unwrap(),
            "abc123"
        );
    }

    /// Ensures callback parsing surfaces OAuth errors from the query string.
    #[test]
    fn parse_openai_codex_callback_prefers_error_description() {
        let path =
            "/auth/callback?state=expected-state&error=access_denied&error_description=Needs+login";
        let error = parse_openai_codex_callback(path, "expected-state").unwrap_err();
        assert_eq!(error.http_status, 400);
        assert_eq!(error.browser_message, "Login failed");
        assert!(error.error.to_string().contains("Needs login"));
    }

    /// Ensures callback parsing rejects the wrong state value.
    #[test]
    fn parse_openai_codex_callback_rejects_state_mismatch() {
        let path = "/auth/callback?state=wrong-state&code=abc123";
        let error = parse_openai_codex_callback(path, "expected-state").unwrap_err();
        assert_eq!(error.http_status, 400);
        assert_eq!(error.browser_message, "State mismatch");
        assert!(error.error.to_string().contains("state mismatch"));
    }

    /// Ensures callback parsing rejects callbacks that do not carry a code.
    #[test]
    fn parse_openai_codex_callback_rejects_missing_code() {
        let path = "/auth/callback?state=expected-state";
        let error = parse_openai_codex_callback(path, "expected-state").unwrap_err();
        assert_eq!(error.http_status, 400);
        assert_eq!(error.browser_message, "Missing code");
        assert!(error.error.to_string().contains("missing code"));
    }

    /// Ensures the advertised redirect URI matches the actual loopback bind address.
    #[tokio::test]
    async fn bind_openai_codex_callback_uses_bound_ipv4_redirect_uri() {
        let (listener, redirect_uri) = bind_openai_codex_callback().await.unwrap();
        let local_addr = listener.local_addr().unwrap();

        assert_eq!(local_addr.ip().to_string(), "127.0.0.1");
        assert_eq!(
            redirect_uri,
            format!("http://127.0.0.1:{}/auth/callback", local_addr.port())
        );
    }
}
