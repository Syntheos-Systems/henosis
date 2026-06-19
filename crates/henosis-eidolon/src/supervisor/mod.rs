//! The eidolon supervisor: watches agent session JSONL directories, detects rule violations,
//! and publishes them as typed events on the in-process Axon bus (Story 2.5).
//!
//! Ported (copy-and-own, the agent-forge pattern) from Kleos `eidolon-supervisor` (741 LOC).
//! Deviations from Kleos, each deliberate:
//! - Alerting is an Axon [`ViolationDetected`] event instead of three Kleos HTTP calls
//!   (`/inbox`, `/axon/publish`, `/supervisor/inject`); Henosis consumers subscribe on the bus
//!   (a Rift-backed operator alert arrives in Phase 4, the inject path with sessions/Synapse).
//! - Rule regexes compile ONCE at construction, and an invalid pattern is a construction error
//!   instead of a silently skipped check.
//! - The file-scope check is wired (Kleos shipped it dead); an empty allow-list disables it.
//! - Cooldowns use `std::time::Instant` (monotonic) instead of wall-clock chrono.
//! - The promise-vs-action drift check is NOT ported: it was a no-op placeholder in Kleos and
//!   its real implementation needs cross-turn session state (Phase 4 / T1 territory).

mod retry_loop;
mod rule_match;
mod scope;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lru::LruCache;
use serde::{Deserialize, Serialize};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TenantId, TypedEvent};

use crate::policy::EidolonError;
use retry_loop::RetryTracker;

/// Cap on the session-file -> read-offset cache: without it the supervisor's heap grows
/// linearly with every distinct session file ever seen.
const POSITIONS_CAPACITY: usize = 2048;

/// The channel supervisor violation events ride on.
pub const SUPERVISION_CHANNEL: &str = "supervision";

/// One supervision rule: what to look for and how loudly to flag it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Stable rule identifier (used for cooldown keying and event attribution).
    pub id: String,
    /// Which detector evaluates this rule.
    pub check_type: CheckType,
    /// The regex pattern (`RuleMatch` rules; ignored by other detectors).
    pub pattern: String,
    /// How serious a hit is.
    pub severity: Severity,
    /// Seconds after a hit during which this rule stays silent.
    pub cooldown_secs: u64,
    /// The human-readable alert message.
    pub message: String,
}

/// Which detector evaluates a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckType {
    /// Regex over the entry's text surfaces.
    RuleMatch,
    /// Identical-command repetition.
    RetryLoop,
    /// Edits outside the allowed path set.
    ScopeViolation,
    /// Promise-vs-action drift (reserved; not yet implemented, see module docs).
    Drift,
}

/// How serious a supervision violation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth a note.
    Info,
    /// Worth attention.
    Warning,
    /// Stop-the-line.
    Critical,
}

impl Severity {
    /// The canonical wire token for this severity.
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }
}

/// One detected violation, before it becomes an event.
#[derive(Debug)]
pub struct Violation {
    /// The rule that fired.
    pub rule_id: String,
    /// How serious it is.
    pub severity: Severity,
    /// The alert message.
    pub message: String,
    /// The matched text (truncated).
    pub context: String,
    /// The session id from the JSONL line, when present.
    pub session_id: Option<String>,
}

/// The typed event a violation is published as.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ViolationDetected {
    /// The rule that fired.
    pub rule_id: String,
    /// Severity token (`info`/`warning`/`critical`).
    pub severity: String,
    /// The alert message.
    pub message: String,
    /// The matched text (truncated).
    pub context: String,
    /// The session id from the JSONL line, when present.
    pub session_id: Option<String>,
}

impl TypedEvent for ViolationDetected {
    const CHANNEL: &'static str = SUPERVISION_CHANNEL;
    const KIND: &'static str = "violation.detected";
}

/// A rule with its regex compiled once at supervisor construction.
pub(crate) struct CompiledRule {
    /// The rule as configured.
    pub(crate) rule: Rule,
    /// The compiled pattern (`RuleMatch` rules only; `None` for other detectors).
    pub(crate) regex: Option<regex::Regex>,
}

/// The built-in rule set, matching the Kleos defaults.
pub fn default_rules() -> Vec<Rule> {
    vec![
        Rule {
            id: "no-force-push".into(),
            check_type: CheckType::RuleMatch,
            pattern: r"git\s+push\s+.*--force".into(),
            severity: Severity::Critical,
            cooldown_secs: 300,
            message: "Force push detected".into(),
        },
        Rule {
            id: "no-reboot".into(),
            check_type: CheckType::RuleMatch,
            pattern: r"reboot|shutdown|systemctl\s+(reboot|poweroff)".into(),
            severity: Severity::Critical,
            cooldown_secs: 600,
            message: "Reboot or shutdown command detected".into(),
        },
        Rule {
            id: "retry-loop".into(),
            check_type: CheckType::RetryLoop,
            pattern: String::new(),
            severity: Severity::Warning,
            cooldown_secs: 120,
            message: "Agent stuck in retry loop (3+ identical commands)".into(),
        },
        Rule {
            id: "em-dash-usage".into(),
            check_type: CheckType::RuleMatch,
            pattern: "\u{2014}".into(),
            severity: Severity::Info,
            cooldown_secs: 60,
            message: "Em dash used in output, should use -- instead".into(),
        },
    ]
}

/// Parse a JSON rule file's content (the `EIDOLON_SUPERVISOR_CONFIG` format from Kleos).
pub fn rules_from_json(content: &str) -> Result<Vec<Rule>, EidolonError> {
    serde_json::from_str(content)
        .map_err(|e| EidolonError::InvalidPolicy(format!("supervisor rules parse: {e}")))
}

/// Supervisor configuration: where to watch, what to flag, and whose identity events carry.
pub struct SupervisorConfig {
    /// The directory of session JSONL files to watch (recursively).
    pub watch_dir: PathBuf,
    /// The supervision rules.
    pub rules: Vec<Rule>,
    /// Path prefixes edits are allowed under; empty disables the scope check.
    pub allowed_paths: Vec<String>,
    /// Tenant the violation events belong to.
    pub tenant: TenantId,
    /// Principal the events are attributed to (the supervisor's own identity).
    pub principal: PrincipalId,
}

/// The supervisor: incremental JSONL reader + detectors + cooldowns + Axon publishing.
///
/// Construct with [`Supervisor::new`] (validates every rule pattern), drive either manually via
/// [`Supervisor::scan_file`] (tests, one-shot sweeps) or as a long-running watcher via
/// [`Supervisor::run`].
pub struct Supervisor {
    /// The validated config (rules kept compiled in `rules`).
    config: SupervisorConfig,
    /// Rules with their regexes compiled once.
    rules: Vec<CompiledRule>,
    /// The bus violations are published onto.
    bus: Arc<AxonBus>,
    /// Per-file read offsets, LRU-bounded.
    positions: LruCache<PathBuf, u64>,
    /// Identical-command repetition state.
    retry_tracker: RetryTracker,
    /// Per-rule silence deadlines.
    cooldowns: HashMap<String, Instant>,
}

impl Supervisor {
    /// Build the supervisor, compiling every `RuleMatch` pattern. An invalid pattern is a
    /// construction error: a rule that cannot run must not be silently skipped.
    pub fn new(config: SupervisorConfig, bus: Arc<AxonBus>) -> Result<Self, EidolonError> {
        let mut rules = Vec::with_capacity(config.rules.len());
        for rule in &config.rules {
            // Fail-closed on unrunnable check types. Only RuleMatch and RetryLoop are
            // rule-driven (rule_match.rs / retry_loop.rs iterate the rule set). ScopeViolation is
            // driven by `allowed_paths`, not per-rule, and Drift has no detector (see module
            // docs), so a rule declaring either could never fire. Accepting it silently is the
            // same trap as a non-compiling pattern: reject it here rather than pretend it runs.
            match rule.check_type {
                CheckType::RuleMatch | CheckType::RetryLoop => {}
                CheckType::ScopeViolation | CheckType::Drift => {
                    return Err(EidolonError::InvalidPolicy(format!(
                        "rule {:?} uses check_type {:?}, which has no rule-driven detector \
                         (ScopeViolation is configured via allowed_paths; Drift is not \
                         implemented). A rule that cannot run must not be silently accepted.",
                        rule.id, rule.check_type
                    )));
                }
            }
            let regex = if matches!(rule.check_type, CheckType::RuleMatch) {
                Some(regex::Regex::new(&rule.pattern).map_err(|e| {
                    EidolonError::InvalidPolicy(format!(
                        "rule {:?} pattern does not compile: {e}",
                        rule.id
                    ))
                })?)
            } else {
                None
            };
            rules.push(CompiledRule {
                rule: rule.clone(),
                regex,
            });
        }
        let positions = LruCache::new(
            NonZeroUsize::new(POSITIONS_CAPACITY).expect("capacity constant is non-zero"),
        );
        Ok(Self {
            config,
            rules,
            bus,
            positions,
            retry_tracker: RetryTracker::new(),
            cooldowns: HashMap::new(),
        })
    }

    /// Read any new JSONL content of `path` (incremental; restarts on truncation), run every
    /// detector over the new tool entries, and publish one [`ViolationDetected`] event per
    /// non-cooled-down violation. Returns how many events were published.
    pub fn scan_file(&mut self, path: &Path) -> Result<usize, EidolonError> {
        let entries = read_new_entries(path, &mut self.positions)
            .map_err(|e| EidolonError::InvalidPolicy(format!("read {path:?}: {e}")))?;
        let mut published = 0;
        for entry in entries {
            let session_id = extract_session_id(&entry);
            let mut violations = Vec::new();
            violations.extend(rule_match::check(&entry, &self.rules));
            violations.extend(self.retry_tracker.check(&entry, &self.rules));
            violations.extend(scope::check_file_scope(&entry, &self.config.allowed_paths));

            for mut violation in violations {
                if self.is_cooled_down(&violation.rule_id) {
                    continue;
                }
                violation.session_id = session_id.clone();
                tracing::warn!(
                    rule = %violation.rule_id,
                    severity = violation.severity.as_str(),
                    session_id = ?violation.session_id,
                    message = %violation.message,
                    "supervisor violation detected"
                );
                let event = ViolationDetected {
                    rule_id: violation.rule_id.clone(),
                    severity: violation.severity.as_str().to_string(),
                    message: violation.message,
                    context: violation.context,
                    session_id: violation.session_id,
                };
                // Best-effort fanout, like every other kernel emitter: the warn log above is
                // the local record; reach 0 (no subscriber yet) is not an error.
                if let Err(err) =
                    self.bus
                        .publish_event(&event, self.config.tenant, self.config.principal)
                {
                    tracing::warn!(error = %err, "failed to publish supervisor violation");
                } else {
                    published += 1;
                }
                self.set_cooldown(&violation.rule_id);
            }
        }
        Ok(published)
    }

    /// Watch the configured directory (recursively, debounced) and [`Supervisor::scan_file`]
    /// every changed `.jsonl` file, forever. Waits for the directory if it does not exist yet.
    pub async fn run(mut self) {
        let watch_dir = self.config.watch_dir.clone();
        if !watch_dir.exists() {
            tracing::warn!(path = %watch_dir.display(), "supervisor watch dir missing, waiting for it");
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if watch_dir.exists() {
                    break;
                }
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        // The debouncer owns a watcher thread; blocking_send is correct there (not async).
        let mut debouncer = match notify_debouncer_mini::new_debouncer(
            Duration::from_millis(500),
            move |res: notify_debouncer_mini::DebounceEventResult| {
                if let Ok(events) = res {
                    for event in events {
                        let _ = tx.blocking_send(event);
                    }
                }
            },
        ) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, "supervisor cannot create file watcher; not supervising");
                return;
            }
        };
        if let Err(e) = debouncer
            .watcher()
            .watch(&watch_dir, notify::RecursiveMode::Recursive)
        {
            tracing::error!(
                path = %watch_dir.display(),
                error = %e,
                "supervisor cannot watch directory; not supervising"
            );
            return;
        }
        tracing::info!(path = %watch_dir.display(), "supervisor watching for session changes");

        while let Some(event) = rx.recv().await {
            if event.kind != notify_debouncer_mini::DebouncedEventKind::Any {
                continue;
            }
            let path = &event.path;
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if !path.exists() {
                continue;
            }
            if let Err(e) = self.scan_file(path) {
                tracing::warn!(path = %path.display(), error = %e, "supervisor scan failed");
            }
        }
    }

    /// True when `rule_id` is still inside its cooldown window.
    fn is_cooled_down(&self, rule_id: &str) -> bool {
        self.cooldowns
            .get(rule_id)
            .is_some_and(|until| Instant::now() < *until)
    }

    /// Start `rule_id`'s cooldown window (its configured seconds; 60 when the rule is unknown,
    /// which only the hardcoded scope-violation id is).
    fn set_cooldown(&mut self, rule_id: &str) {
        let secs = self
            .rules
            .iter()
            .find(|c| c.rule.id == rule_id)
            .map(|c| c.rule.cooldown_secs)
            .unwrap_or(60);
        self.cooldowns.insert(
            rule_id.to_string(),
            Instant::now() + Duration::from_secs(secs),
        );
    }
}

/// Pull a session id out of a JSONL entry (`sessionId` everywhere current; `session_id` in
/// older transcripts).
fn extract_session_id(entry: &serde_json::Value) -> Option<String> {
    let obj = entry.as_object()?;
    obj.get("sessionId")
        .or_else(|| obj.get("session_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Read `path`'s lines from the cached offset forward, returning the tool-use entries and
/// updating the cache. A shrunken file (rotation/truncation) rescans from the start.
fn read_new_entries(
    path: &Path,
    positions: &mut LruCache<PathBuf, u64>,
) -> Result<Vec<serde_json::Value>, std::io::Error> {
    let path_buf = path.to_path_buf();
    // peek() reads the offset without promoting to MRU; the put() below does the promotion.
    let last_pos = positions.peek(&path_buf).copied().unwrap_or(0);

    let file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let start_pos = if file_len < last_pos { 0 } else { last_pos };

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(start_pos))?;

    let mut new_pos = start_pos;
    let mut entries = Vec::new();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        new_pos += line.len() as u64 + 1;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            let is_tool = value
                .as_object()
                .map(|o| {
                    o.contains_key("tool_name")
                        || o.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                })
                .unwrap_or(false);
            if is_tool {
                entries.push(value);
            }
        }
    }
    positions.put(path_buf, new_pos);
    Ok(entries)
}

/// Clip `s` to at most `max` bytes (on a char boundary), appending an ellipsis when clipped.
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Tests for construction validation, incremental scanning, detectors, cooldowns, and the
/// watcher loop.
#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A unique temp file path with the given extension.
    fn temp_path(ext: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "eidolon-supervisor-{}.{ext}",
            syntheos_contracts::EventId::new()
        ))
    }

    /// Build a supervisor over default rules and a fresh bus, returning both.
    fn supervisor() -> (Supervisor, Arc<AxonBus>) {
        supervisor_with(default_rules(), Vec::new())
    }

    /// Build a supervisor with explicit rules and allowed paths.
    fn supervisor_with(rules: Vec<Rule>, allowed_paths: Vec<String>) -> (Supervisor, Arc<AxonBus>) {
        let bus = Arc::new(AxonBus::new());
        let sup = Supervisor::new(
            SupervisorConfig {
                watch_dir: std::env::temp_dir(),
                rules,
                allowed_paths,
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
            },
            bus.clone(),
        )
        .expect("valid config");
        (sup, bus)
    }

    /// Append one JSONL line for a Bash tool use running `cmd` in session `sid`.
    fn append_bash(path: &Path, cmd: &str, sid: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "tool_name": "Bash",
                "tool_input": { "command": cmd },
                "sessionId": sid,
            })
        )
        .expect("write");
    }

    /// The default rules cover the classics and all compile.
    #[test]
    fn default_rules_compile_and_cover_classics() {
        let rules = default_rules();
        assert!(rules.iter().any(|r| r.id == "no-force-push"));
        assert!(rules
            .iter()
            .any(|r| matches!(r.check_type, CheckType::RetryLoop)));
        let (_sup, _bus) = supervisor();
    }

    /// An invalid regex pattern is rejected at construction, not skipped at runtime.
    #[test]
    fn invalid_pattern_rejected_at_construction() {
        let bus = Arc::new(AxonBus::new());
        let err = Supervisor::new(
            SupervisorConfig {
                watch_dir: std::env::temp_dir(),
                rules: vec![Rule {
                    id: "broken".into(),
                    check_type: CheckType::RuleMatch,
                    pattern: "(unclosed".into(),
                    severity: Severity::Info,
                    cooldown_secs: 1,
                    message: "broken".into(),
                }],
                allowed_paths: Vec::new(),
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
            },
            bus,
        )
        .err()
        .expect("invalid pattern must be rejected");
        assert!(matches!(err, EidolonError::InvalidPolicy(_)), "got {err:?}");
    }

    /// A rule whose check_type has no rule-driven detector (Drift, ScopeViolation) is rejected at
    /// construction rather than silently accepted-and-never-run (fail-closed).
    #[test]
    fn unrunnable_check_type_rejected_at_construction() {
        for ct in [CheckType::Drift, CheckType::ScopeViolation] {
            let bus = Arc::new(AxonBus::new());
            let err = Supervisor::new(
                SupervisorConfig {
                    watch_dir: std::env::temp_dir(),
                    rules: vec![Rule {
                        id: "unrunnable".into(),
                        check_type: ct,
                        pattern: String::new(),
                        severity: Severity::Warning,
                        cooldown_secs: 1,
                        message: "never runs".into(),
                    }],
                    allowed_paths: Vec::new(),
                    tenant: TenantId::new(),
                    principal: PrincipalId::new(),
                },
                bus,
            )
            .err()
            .unwrap_or_else(|| panic!("{ct:?} rule must be rejected at construction"));
            assert!(matches!(err, EidolonError::InvalidPolicy(_)), "got {err:?}");
        }
    }

    /// A force-push entry produces one typed violation event carrying the session id.
    #[tokio::test]
    async fn scan_publishes_rule_match_violation() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = temp_path("jsonl");
        append_bash(&path, "git push origin main --force", "sess-1");

        let published = sup.scan_file(&path).expect("scan");
        assert_eq!(published, 1);
        let event = rx.recv().await.expect("violation event");
        assert_eq!(event.rule_id, "no-force-push");
        assert_eq!(event.severity, "critical");
        assert_eq!(event.session_id.as_deref(), Some("sess-1"));
        let _ = std::fs::remove_file(&path);
    }

    /// Scanning is incremental: already-seen lines are not re-processed.
    #[tokio::test]
    async fn scan_is_incremental() {
        let (mut sup, _bus) = supervisor();
        let path = temp_path("jsonl");
        append_bash(&path, "git push --force", "s");
        assert_eq!(sup.scan_file(&path).expect("scan 1"), 1);

        // Re-scan with no new content: nothing published (also proves cooldown is not what
        // suppressed it -- there is simply nothing new to read).
        assert_eq!(sup.scan_file(&path).expect("scan 2"), 0);

        // New clean content: still nothing.
        append_bash(&path, "ls -la", "s");
        assert_eq!(sup.scan_file(&path).expect("scan 3"), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// A truncated (rotated) file is rescanned from the start.
    #[tokio::test]
    async fn truncated_file_rescans_from_start() {
        let (mut sup, _bus) = supervisor();
        let path = temp_path("jsonl");
        append_bash(
            &path,
            "git push --force && sleep 1 && echo padding-padding",
            "s",
        );
        assert_eq!(sup.scan_file(&path).expect("scan 1"), 1);

        // Rotate: rewrite the file shorter than the cached offset, with a fresh violation
        // under a different rule (no-force-push is now cooling down).
        std::fs::write(&path, "").expect("truncate");
        append_bash(&path, "sudo reboot", "s");
        assert_eq!(
            sup.scan_file(&path).expect("scan 2"),
            1,
            "rotation must rescan from offset 0"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Three identical commands trip the retry-loop detector.
    #[tokio::test]
    async fn retry_loop_fires_on_third_identical_command() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = temp_path("jsonl");
        append_bash(&path, "cargo test -p broken", "s");
        append_bash(&path, "cargo test -p broken", "s");
        assert_eq!(
            sup.scan_file(&path).expect("scan"),
            0,
            "two repeats stay quiet"
        );
        append_bash(&path, "cargo test -p broken", "s");
        assert_eq!(sup.scan_file(&path).expect("scan"), 1);
        let event = rx.recv().await.expect("event");
        assert_eq!(event.rule_id, "retry-loop");
        let _ = std::fs::remove_file(&path);
    }

    /// A rule on cooldown stays silent for repeat hits.
    #[tokio::test]
    async fn cooldown_suppresses_repeat_violations() {
        let (mut sup, _bus) = supervisor();
        let path = temp_path("jsonl");
        append_bash(&path, "git push --force", "s");
        assert_eq!(sup.scan_file(&path).expect("scan"), 1);
        append_bash(&path, "git push --force # again", "s");
        assert_eq!(
            sup.scan_file(&path).expect("scan"),
            0,
            "no-force-push has a 300s cooldown"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// With allowed_paths configured, an out-of-scope edit fires and an in-scope one does not.
    #[tokio::test]
    async fn scope_check_wired_when_allow_list_present() {
        let (mut sup, bus) =
            supervisor_with(default_rules(), vec!["/home/user/projects/henosis".into()]);
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = temp_path("jsonl");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "tool_name": "Edit",
                "tool_input": { "file_path": "/etc/passwd", "old_string": "x" },
                "sessionId": "s",
            })
        )
        .expect("write");
        writeln!(
            f,
            "{}",
            serde_json::json!({
                "tool_name": "Edit",
                "tool_input": { "file_path": "/home/user/projects/henosis/README.md" },
                "sessionId": "s",
            })
        )
        .expect("write");
        drop(f);
        assert_eq!(sup.scan_file(&path).expect("scan"), 1);
        let event = rx.recv().await.expect("event");
        assert_eq!(event.rule_id, "scope-violation");
        assert!(
            event.context.contains("/etc/passwd"),
            "context: {}",
            event.context
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Non-tool entries (plain assistant messages) are ignored.
    #[tokio::test]
    async fn non_tool_entries_ignored() {
        let (mut sup, _bus) = supervisor();
        let path = temp_path("jsonl");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(
            f,
            "{}",
            serde_json::json!({ "type": "assistant", "text": "git push --force is bad" })
        )
        .expect("write");
        drop(f);
        assert_eq!(sup.scan_file(&path).expect("scan"), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// The JSON rule-file format parses (and rejects garbage).
    #[test]
    fn rules_from_json_roundtrip() {
        let json = serde_json::to_string(&default_rules()).expect("serialize");
        let back = rules_from_json(&json).expect("parse");
        assert_eq!(back.len(), default_rules().len());
        assert!(rules_from_json("not json").is_err());
    }

    /// End-to-end: the watcher loop sees a new session file appear and publishes the violation.
    #[tokio::test(flavor = "multi_thread")]
    async fn watcher_detects_change_and_publishes() {
        let watch_dir = std::env::temp_dir().join(format!(
            "eidolon-supervisor-watch-{}",
            syntheos_contracts::EventId::new()
        ));
        std::fs::create_dir_all(&watch_dir).expect("mkdir");
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let sup = Supervisor::new(
            SupervisorConfig {
                watch_dir: watch_dir.clone(),
                rules: default_rules(),
                allowed_paths: Vec::new(),
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
            },
            bus.clone(),
        )
        .expect("valid config");
        let task = tokio::spawn(sup.run());

        // Give the watcher a moment to install, then write the violating session line.
        tokio::time::sleep(Duration::from_millis(300)).await;
        append_bash(
            &watch_dir.join("session-1.jsonl"),
            "git push --force",
            "live-1",
        );

        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("watcher must publish within 10s")
            .expect("event");
        assert_eq!(event.rule_id, "no-force-push");
        assert_eq!(event.session_id.as_deref(), Some("live-1"));

        task.abort();
        let _ = std::fs::remove_dir_all(&watch_dir);
    }
}
