//! The eidolon supervisor: watches agent session JSONL directories, detects rule violations,
//! and publishes them as typed events on the in-process Axon bus.
//!
//! Alerts are emitted as [`ViolationDetected`] events for subscribers to handle. Rule regexes
//! compile once at construction, file-scope checking is enabled by a non-empty allow-list, and
//! cooldowns use monotonic [`std::time::Instant`] values.
//! Critical internal violations mean the supervisor could not inspect all available content and
//! consumers must treat supervision coverage as incomplete until the source is scanned again.

mod retry_loop;
mod rule_match;
mod scope;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::num::NonZeroUsize;
use std::path::{Component, Path, PathBuf};
#[cfg(not(any(unix, windows)))]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cap_primitives::fs::FollowSymlinks;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use syntheos_axon::AxonBus;
use syntheos_contracts::{PrincipalId, TenantId, TypedEvent};

use crate::policy::EidolonError;
use retry_loop::RetryTracker;

/// Cap on the session-file -> read-offset cache: without it the supervisor's heap grows
/// linearly with every distinct session file ever seen.
const POSITIONS_CAPACITY: usize = 256;

/// Maximum accepted size of one session JSONL record.
const MAX_SESSION_LINE_BYTES: usize = 1024 * 1024;

/// Maximum session-file bytes consumed by one scan batch.
const MAX_SESSION_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Maximum complete JSONL records examined by one scan batch.
const MAX_SESSION_SCAN_RECORDS: usize = 4096;

/// Maximum unique session files allowed to hold a queued scan continuation.
const MAX_PENDING_SCAN_PATHS: usize = 256;

/// Maximum filesystem entries inspected by one authoritative retained-root discovery.
const MAX_SESSION_DISCOVERY_ENTRIES: usize = 4096;

/// Maximum retained-root directory nesting accepted by authoritative discovery.
const MAX_SESSION_DISCOVERY_DEPTH: usize = 64;

/// Maximum encoded length of one retained-root-relative discovery path.
const MAX_SESSION_DISCOVERY_PATH_BYTES: usize = 4096;

/// Maximum aggregate encoded path bytes retained by one discovery result.
const MAX_SESSION_DISCOVERY_TOTAL_PATH_BYTES: usize = 1024 * 1024;

/// Rule id emitted when a record is too large to inspect safely.
const OVERSIZED_SESSION_RECORD_RULE_ID: &str = "oversized-session-record";

/// Rule id emitted when bounded continuation capacity cannot retain more work.
const SUPERVISOR_OVERLOAD_RULE_ID: &str = "supervisor-overload";

/// Rule id emitted when retained-root discovery cannot guarantee complete coverage.
const SUPERVISOR_DISCOVERY_FAILURE_RULE_ID: &str = "supervisor-discovery-failure";

/// Rule id emitted when one discovered session file cannot be inspected.
const SUPERVISOR_SCAN_FAILURE_RULE_ID: &str = "supervisor-scan-failure";

/// Rule id emitted when the configured watch path no longer names the retained root.
const WATCH_ROOT_IDENTITY_LOST_RULE_ID: &str = "watch-root-identity-lost";

/// Maximum time a quiet watcher may go without authoritative retained-root discovery.
const WATCH_ROOT_VERIFY_INTERVAL: Duration = Duration::from_secs(5);

/// Process-local identity source for platforms without stable opened-file handles.
#[cfg(not(any(unix, windows)))]
static NEXT_FALLBACK_FILE_IDENTITY: AtomicU64 = AtomicU64::new(1);

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
    /// Reserved promise-vs-action drift detector; the supervisor currently ignores this value.
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

/// Implements Severity wire-format conversion.
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

/// Implements Axon event metadata for detected violations.
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

/// A retained capability and the stable lexical prefixes accepted for event paths.
struct WatchRoot {
    /// Directory capability that every session-file open is resolved beneath.
    directory: Dir,
    /// Identity of the retained directory used to verify the pathname-based watcher.
    identity: FileIdentity,
    /// Absolute lexical form of the configured watch path.
    configured_prefix: PathBuf,
    /// Resolved form accepted when a watcher reports canonical paths.
    resolved_prefix: PathBuf,
}

/// Explicit resource ceilings for one retained-root session-file discovery.
#[derive(Debug, Clone, Copy)]
struct SessionDiscoveryLimits {
    /// Maximum directory entries inspected across the complete traversal.
    max_entries: usize,
    /// Maximum session JSONL paths returned by the traversal.
    max_paths: usize,
    /// Maximum directory nesting beneath the retained root.
    max_depth: usize,
    /// Maximum encoded bytes in one retained-root-relative path.
    max_path_bytes: usize,
    /// Maximum aggregate encoded bytes in all returned relative paths.
    max_total_path_bytes: usize,
}

/// Mutable accounting for one bounded retained-root traversal.
#[derive(Debug, Default)]
struct SessionDiscoveryState {
    /// Number of directory entries inspected so far.
    entries_seen: usize,
    /// Aggregate encoded bytes in discovered relative paths.
    path_bytes: usize,
    /// Stable configured-prefix paths for regular JSONL files.
    paths: Vec<PathBuf>,
}

/// Production resource ceilings for retained-root discovery.
const SESSION_DISCOVERY_LIMITS: SessionDiscoveryLimits = SessionDiscoveryLimits {
    max_entries: MAX_SESSION_DISCOVERY_ENTRIES,
    max_paths: MAX_PENDING_SCAN_PATHS,
    max_depth: MAX_SESSION_DISCOVERY_DEPTH,
    max_path_bytes: MAX_SESSION_DISCOVERY_PATH_BYTES,
    max_total_path_bytes: MAX_SESSION_DISCOVERY_TOTAL_PATH_BYTES,
};

/// Stable Unix identity for an opened file, retaining its handle against inode reuse.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct FileIdentity(same_file::Handle);

/// Stable Windows identity for an opened file, including the full ReFS-safe 128-bit identifier.
#[cfg(windows)]
#[derive(Debug)]
struct FileIdentity {
    /// Retained handle that prevents identifier reuse while the cursor is cached.
    _retained: File,
    /// Volume serial number paired with the file identifier.
    volume_serial_number: u64,
    /// Full file identifier returned by `FILE_ID_INFO`.
    file_id: [u8; 16],
}

/// Compares Windows file identities without comparing their retained handles.
#[cfg(windows)]
impl PartialEq for FileIdentity {
    /// Match only when both the volume and full 128-bit file identifier match.
    fn eq(&self, other: &Self) -> bool {
        self.volume_serial_number == other.volume_serial_number && self.file_id == other.file_id
    }
}

/// Marks Windows file identity comparison as an equivalence relation.
#[cfg(windows)]
impl Eq for FileIdentity {}

/// Deliberately unique identity on unsupported platforms so offsets never survive a rescan.
#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity(u64);

/// Incremental read state for one trusted append-only relative session path.
struct ReadPosition {
    /// Identity of the file whose offset was recorded.
    identity: FileIdentity,
    /// First byte not yet consumed from the file.
    offset: u64,
    /// Whether the next bytes continue an oversized record being discarded.
    discarding_oversized_line: bool,
}

/// One bounded read batch plus whether the retained file still has pending complete data.
struct ReadBatch {
    /// Parsed tool-use entries from this batch.
    entries: Vec<serde_json::Value>,
    /// Number of oversized records first encountered in this batch.
    oversized_records: usize,
    /// Whether another bounded batch should be scheduled immediately.
    has_more: bool,
    /// First byte not yet committed after this batch.
    committed_offset: u64,
    /// File length observed after this batch completed.
    observed_len: u64,
}

/// Detector and cursor outcome for one bounded scan batch.
struct ScanBatchOutcome {
    /// Number of violation events successfully published.
    published: usize,
    /// Whether the opened file still has pending complete data.
    has_more: bool,
    /// First byte not yet committed after this batch.
    committed_offset: u64,
    /// File length observed after this batch completed.
    observed_len: u64,
}

/// Result of consuming one bounded JSONL record or one bounded discard segment.
struct SessionLineRead {
    /// Record bytes when the line stayed within the configured limit.
    bytes: Option<Vec<u8>>,
    /// Bytes consumed from the descriptor during this operation.
    consumed: usize,
    /// Whether a newline completed the record.
    terminated: bool,
    /// Whether a later batch must continue discarding this oversized record.
    discarding: bool,
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
    /// The directory of trusted append-only session JSONL files to watch recursively.
    ///
    /// The retained capability prevents path escape, replacement, and special-file attacks.
    /// The cursor assumes the session producer never rewrites bytes before the append offset.
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
    /// Lazily acquired watch-root capability, retained for the supervisor lifetime.
    watch_root: Option<WatchRoot>,
    /// Per-file read offsets, LRU-bounded.
    positions: LruCache<PathBuf, ReadPosition>,
    /// Identical-command repetition state.
    retry_tracker: RetryTracker,
    /// Per-rule silence deadlines.
    cooldowns: HashMap<String, Instant>,
}

/// Implements supervisor construction, scanning, and event publication.
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
            watch_root: None,
            positions,
            retry_tracker: RetryTracker::new(),
            cooldowns: HashMap::new(),
        })
    }

    /// Acquire the configured watch-root capability once and retain it without replacement.
    fn ensure_watch_root(&mut self) -> Result<(), std::io::Error> {
        if self.watch_root.is_none() {
            self.watch_root = Some(WatchRoot::open(&self.config.watch_dir)?);
        }
        Ok(())
    }

    /// Verify that the configured path still names the retained watch root, publishing a critical
    /// violation when supervision coverage can no longer be guaranteed.
    fn verify_retained_watch_root(&mut self, watch_dir: &Path) -> bool {
        let result = self
            .watch_root
            .as_ref()
            .expect("watch root is present after successful acquisition")
            .matches_current_path(watch_dir);
        match result {
            Ok(true) => true,
            Ok(false) => {
                tracing::error!(
                    path = %watch_dir.display(),
                    "supervisor watch-directory identity no longer matches retained authority"
                );
                self.publish_violation(Violation {
                    rule_id: WATCH_ROOT_IDENTITY_LOST_RULE_ID.to_string(),
                    severity: Severity::Critical,
                    message: "Supervisor watch-directory identity changed; supervision stopped"
                        .to_string(),
                    context: truncate(&watch_dir.display().to_string(), 500),
                    session_id: None,
                });
                false
            }
            Err(error) => {
                tracing::error!(
                    path = %watch_dir.display(),
                    %error,
                    "supervisor cannot verify retained watch-directory identity"
                );
                self.publish_violation(Violation {
                    rule_id: WATCH_ROOT_IDENTITY_LOST_RULE_ID.to_string(),
                    severity: Severity::Critical,
                    message:
                        "Supervisor could not verify watch-directory identity; supervision stopped"
                            .to_string(),
                    context: truncate(&format!("{}: {error}", watch_dir.display()), 500),
                    session_id: None,
                });
                false
            }
        }
    }

    /// Discover and queue every regular JSONL file through the retained root capability.
    fn enqueue_discovered_paths(
        &mut self,
        pending: &mut VecDeque<PathBuf>,
        pending_paths: &mut HashSet<PathBuf>,
    ) -> bool {
        let discovered = match self
            .watch_root
            .as_ref()
            .expect("watch root is present after successful acquisition")
            .discover_session_paths(SESSION_DISCOVERY_LIMITS)
        {
            Ok(paths) => paths,
            Err(error) => {
                self.publish_discovery_failure(error.to_string());
                return false;
            }
        };
        for path in discovered {
            if pending_paths.contains(&path) {
                continue;
            }
            if pending.len() >= MAX_PENDING_SCAN_PATHS {
                self.publish_discovery_failure(format!(
                    "authoritative discovery exceeded the {MAX_PENDING_SCAN_PATHS}-path scan queue"
                ));
                return false;
            }
            pending_paths.insert(path.clone());
            pending.push_back(path);
        }
        true
    }

    /// Publish a critical event explaining why retained-root discovery lost coverage.
    fn publish_discovery_failure(&mut self, reason: String) {
        tracing::error!(%reason, "supervisor retained-root discovery failed");
        self.publish_violation(Violation {
            rule_id: SUPERVISOR_DISCOVERY_FAILURE_RULE_ID.to_string(),
            severity: Severity::Critical,
            message: "Supervisor could not complete authoritative session discovery; supervision \
                      stopped"
                .to_string(),
            context: truncate(&reason, 500),
            session_id: None,
        });
    }

    /// Publish a critical but recoverable event for one watcher-origin session path.
    fn publish_scan_failure(&mut self, path: &Path, reason: String) {
        tracing::error!(path = %path.display(), %reason, "supervisor session scan failed");
        self.publish_violation(Violation {
            rule_id: SUPERVISOR_SCAN_FAILURE_RULE_ID.to_string(),
            severity: Severity::Critical,
            message:
                "Supervisor could not inspect a watcher-reported session file; periodic retry remains \
                 active"
                    .to_string(),
            context: truncate(&format!("{}: {reason}", path.display()), 500),
            session_id: None,
        });
    }

    /// Scan one bounded batch and report both publication count and pending backlog state.
    fn scan_file_batch(&mut self, path: &Path) -> Result<ScanBatchOutcome, EidolonError> {
        self.ensure_watch_root()
            .map_err(|e| EidolonError::InvalidPolicy(format!("open watch root: {e}")))?;
        let watch_root = self
            .watch_root
            .as_ref()
            .expect("watch root is present after successful acquisition");
        let batch = read_new_entries(path, watch_root, &mut self.positions)
            .map_err(|e| EidolonError::InvalidPolicy(format!("read {path:?}: {e}")))?;
        let has_more = batch.has_more;
        let committed_offset = batch.committed_offset;
        let observed_len = batch.observed_len;
        let entries = batch.entries;
        let mut published = 0;
        for _ in 0..batch.oversized_records {
            published += self.publish_violation(Violation {
                rule_id: OVERSIZED_SESSION_RECORD_RULE_ID.to_string(),
                severity: Severity::Critical,
                message: format!(
                    "Session record exceeded {MAX_SESSION_LINE_BYTES} bytes and was not inspected"
                ),
                context: truncate(&path.display().to_string(), 500),
                session_id: None,
            });
        }
        for entry in entries {
            let session_id = extract_session_id(&entry);
            let mut violations = Vec::new();
            violations.extend(rule_match::check(&entry, &self.rules));
            violations.extend(self.retry_tracker.check(&entry, &self.rules));
            violations.extend(scope::check_file_scope(&entry, &self.config.allowed_paths));

            for mut violation in violations {
                violation.session_id = session_id.clone();
                published += self.publish_violation(violation);
            }
        }
        Ok(ScanBatchOutcome {
            published,
            has_more,
            committed_offset,
            observed_len,
        })
    }

    /// Read every complete record present in the first observed snapshot of `path`, run every
    /// detector, and publish one [`ViolationDetected`] event per non-cooled-down violation.
    ///
    /// Each internal batch has fixed memory and record budgets. Content appended after the first
    /// batch's length observation does not extend the target snapshot, so a hot append-only
    /// producer cannot keep this synchronous method running forever. Later content may remain for
    /// another explicit call or watcher event. Returns how many events were successfully published.
    ///
    /// The session producer is a trusted append-only writer. Replacement and truncation reset
    /// the cursor, but in-place rewrites before the retained offset are outside this API's trust
    /// boundary.
    pub fn scan_file(&mut self, path: &Path) -> Result<usize, EidolonError> {
        let mut batch = self.scan_file_batch(path)?;
        let snapshot_end = batch.observed_len;
        let mut published = batch.published;
        while batch.has_more && batch.committed_offset < snapshot_end {
            let previous_offset = batch.committed_offset;
            batch = self.scan_file_batch(path)?;
            published += batch.published;
            if batch.has_more && batch.committed_offset <= previous_offset {
                return Err(EidolonError::InvalidPolicy(format!(
                    "read {path:?}: session scan made no forward progress"
                )));
            }
        }
        Ok(published)
    }

    /// Scan the retained directory authoritatively and use recursive watcher events to reduce
    /// detection latency between bounded scans.
    ///
    /// The retained-capability scan runs before the event loop and every five seconds, so a
    /// pathname watcher bound to a transient replacement cannot suppress coverage. Waits for the
    /// directory if it does not exist yet.
    pub async fn run(self) {
        self.run_loop(true, WATCH_ROOT_VERIFY_INTERVAL).await;
    }

    /// Drive supervision with explicit accelerator and discovery timing for deterministic tests.
    async fn run_loop(mut self, enable_watcher: bool, discovery_interval: Duration) {
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
        if let Err(error) = self.ensure_watch_root() {
            tracing::error!(
                path = %watch_dir.display(),
                %error,
                "supervisor cannot retain watch-directory authority"
            );
            return;
        }
        let retained_watch_path = self
            .watch_root
            .as_ref()
            .expect("watch root is present after successful acquisition")
            .configured_prefix
            .clone();

        let (tx, mut rx) = tokio::sync::mpsc::channel(100);
        let watcher_tx = tx.clone();
        let watcher_failed = Arc::new(AtomicBool::new(false));
        let callback_failed = Arc::clone(&watcher_failed);
        // The debouncer owns a watcher thread. Its callback never blocks on the async consumer.
        let mut debouncer = if enable_watcher {
            match notify_debouncer_mini::new_debouncer(
                Duration::from_millis(500),
                move |res: notify_debouncer_mini::DebounceEventResult| {
                    forward_watcher_events(&watcher_tx, &callback_failed, res);
                },
            ) {
                Ok(d) => Some(d),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "supervisor file watcher unavailable; retained-root discovery remains active"
                    );
                    None
                }
            }
        } else {
            None
        };
        let watcher_registration_error = debouncer.as_mut().and_then(|active_debouncer| {
            active_debouncer
                .watcher()
                .watch(&retained_watch_path, notify::RecursiveMode::Recursive)
                .err()
        });
        if let Some(e) = watcher_registration_error {
            tracing::warn!(
                path = %retained_watch_path.display(),
                error = %e,
                "supervisor cannot watch directory; retained-root discovery remains active"
            );
            drop(debouncer.take());
        }
        let mut pending = VecDeque::new();
        let mut pending_paths = HashSet::new();
        if !self.verify_retained_watch_root(&retained_watch_path)
            || !self.enqueue_discovered_paths(&mut pending, &mut pending_paths)
        {
            return;
        }
        tracing::info!(
            path = %retained_watch_path.display(),
            initial_paths = pending.len(),
            "supervisor retained-root discovery active"
        );

        let _watcher_channel_guard = tx;
        let mut prefer_pending = false;
        let mut root_identity_interval = tokio::time::interval(discovery_interval);
        root_identity_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        root_identity_interval.tick().await;
        loop {
            let next = tokio::select! {
                next = next_scan_path(
                    &mut rx,
                    &mut pending,
                    &mut pending_paths,
                    &mut prefer_pending,
                ) => next,
                _ = root_identity_interval.tick() => {
                    if watcher_failed.swap(false, Ordering::AcqRel) {
                        tracing::warn!(
                            "supervisor file watcher failed; retained-root discovery remains active"
                        );
                        drop(debouncer.take());
                    }
                    if !self.verify_retained_watch_root(&retained_watch_path)
                        || !self.enqueue_discovered_paths(&mut pending, &mut pending_paths)
                    {
                        return;
                    }
                    continue;
                }
            };
            let Some((path, from_pending)) = next else {
                return;
            };
            if !self.verify_retained_watch_root(&retained_watch_path) {
                return;
            }
            let path = if from_pending {
                path
            } else {
                let root = self
                    .watch_root
                    .as_ref()
                    .expect("watch root is present after successful acquisition");
                match root.relative_path(&path) {
                    Ok(relative) => root.configured_prefix.join(relative),
                    Err(error) => {
                        tracing::warn!(
                            path = %path.display(),
                            %error,
                            "ignored watcher event outside retained root"
                        );
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
            };
            let should_scan = path.extension().and_then(|extension| extension.to_str())
                == Some("jsonl")
                && (from_pending || !pending_paths.contains(&path));
            if should_scan {
                match self.scan_file_batch(&path) {
                    Ok(ScanBatchOutcome { has_more: true, .. }) => {
                        self.enqueue_continuation(path, &mut pending, &mut pending_paths);
                    }
                    Ok(ScanBatchOutcome {
                        has_more: false, ..
                    }) => {}
                    Err(error) => {
                        if from_pending {
                            self.publish_discovery_failure(format!(
                                "authoritative scan failed for {}: {error}",
                                path.display()
                            ));
                            return;
                        }
                        self.publish_scan_failure(&path, error.to_string());
                    }
                }
            }
            tokio::task::yield_now().await;
        }
    }

    /// True when `rule_id` is still inside its cooldown window.
    fn is_cooled_down(&self, rule_id: &str) -> bool {
        self.cooldowns
            .get(rule_id)
            .is_some_and(|until| Instant::now() < *until)
    }

    /// Publish one violation unless its rule is cooling down.
    fn publish_violation(&mut self, violation: Violation) -> usize {
        if self.is_cooled_down(&violation.rule_id) {
            return 0;
        }
        tracing::warn!(
            rule = %violation.rule_id,
            severity = violation.severity.as_str(),
            session_id = ?violation.session_id,
            message = %violation.message,
            "supervisor violation detected"
        );
        let rule_id = violation.rule_id.clone();
        let event = ViolationDetected {
            rule_id: violation.rule_id,
            severity: violation.severity.as_str().to_string(),
            message: violation.message,
            context: violation.context,
            session_id: violation.session_id,
        };
        // Best-effort fanout, like every other kernel emitter: the warn log above is the local
        // record; reach 0 (no subscriber yet) is not an error.
        let published = if let Err(err) =
            self.bus
                .publish_event(&event, self.config.tenant, self.config.principal)
        {
            tracing::warn!(error = %err, "failed to publish supervisor violation");
            0
        } else {
            1
        };
        self.set_cooldown(&rule_id);
        published
    }

    /// Queue one unique continuation or emit a critical overload event when capacity is full.
    fn enqueue_continuation(
        &mut self,
        path: PathBuf,
        pending: &mut VecDeque<PathBuf>,
        pending_paths: &mut HashSet<PathBuf>,
    ) -> usize {
        if pending_paths.contains(&path) {
            return 0;
        }
        if pending.len() < MAX_PENDING_SCAN_PATHS {
            pending_paths.insert(path.clone());
            pending.push_back(path);
            return 0;
        }
        self.publish_violation(Violation {
            rule_id: SUPERVISOR_OVERLOAD_RULE_ID.to_string(),
            severity: Severity::Critical,
            message: format!(
                "Supervisor continuation capacity of {MAX_PENDING_SCAN_PATHS} paths was exhausted"
            ),
            context: truncate(&path.display().to_string(), 500),
            session_id: None,
        })
    }

    /// Start `rule_id`'s cooldown window (its configured seconds; 60 for built-in internal
    /// violations and the hardcoded scope-violation id).
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

/// Forward every debounced filesystem event, including continuous-write notifications, to the
/// bounded async scan queue.
fn forward_watcher_events(
    watcher_tx: &tokio::sync::mpsc::Sender<PathBuf>,
    watcher_failed: &AtomicBool,
    result: notify_debouncer_mini::DebounceEventResult,
) {
    match result {
        Ok(events) => {
            for event in events {
                match watcher_tx.try_send(event.path) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(path)) => {
                        watcher_failed.store(true, Ordering::Release);
                        tracing::warn!(
                            path = %path.display(),
                            "supervisor watcher event queue saturated; disabling accelerator"
                        );
                        break;
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
        Err(error) => {
            watcher_failed.store(true, Ordering::Release);
            tracing::error!(?error, "supervisor filesystem watcher failed");
        }
    }
}

/// Select the next watcher event or bounded continuation without starving either source.
async fn next_scan_path(
    rx: &mut tokio::sync::mpsc::Receiver<PathBuf>,
    pending: &mut VecDeque<PathBuf>,
    pending_paths: &mut HashSet<PathBuf>,
    prefer_pending: &mut bool,
) -> Option<(PathBuf, bool)> {
    if *prefer_pending {
        if let Some(path) = pending.pop_front() {
            pending_paths.remove(&path);
            *prefer_pending = false;
            return Some((path, true));
        }
    }

    match rx.try_recv() {
        Ok(path) => {
            *prefer_pending = true;
            Some((path, false))
        }
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
            if let Some(path) = pending.pop_front() {
                pending_paths.remove(&path);
                *prefer_pending = false;
                Some((path, true))
            } else {
                let path = rx.recv().await?;
                *prefer_pending = true;
                Some((path, false))
            }
        }
        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
            let path = pending.pop_front()?;
            pending_paths.remove(&path);
            *prefer_pending = false;
            Some((path, true))
        }
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

/// Implements retained-root acquisition and safe event-path conversion.
impl WatchRoot {
    /// Open the configured directory once and retain its exact capability.
    #[cfg(any(unix, windows))]
    fn open(path: &Path) -> Result<Self, std::io::Error> {
        let configured_prefix = absolute_configured_path(path)?;
        let directory = Dir::open_ambient_dir(path, ambient_authority())?;
        if !directory.dir_metadata()?.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "supervisor watch root must be a directory",
            ));
        }
        let identity = directory_identity(&directory)?;
        let resolved_prefix = std::fs::canonicalize(path)?;
        let root = Self {
            directory,
            identity,
            configured_prefix,
            resolved_prefix,
        };
        if !root.matches_current_path(path)? {
            return Err(std::io::Error::other(
                "supervisor watch root changed while its capability was acquired",
            ));
        }
        Ok(root)
    }

    /// Fail closed where stable descriptor identity is unavailable.
    #[cfg(not(any(unix, windows)))]
    fn open(_path: &Path) -> Result<Self, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "supervision requires Unix or Windows descriptor identity",
        ))
    }

    /// Verify that `path` still names the retained directory capability.
    fn matches_current_path(&self, path: &Path) -> Result<bool, std::io::Error> {
        let current = Dir::open_ambient_dir(path, ambient_authority())?;
        if !current.dir_metadata()?.is_dir() {
            return Ok(false);
        }
        Ok(directory_identity(&current)? == self.identity)
    }

    /// Collect every regular JSONL path through descriptor-relative no-follow traversal.
    fn discover_session_paths(
        &self,
        limits: SessionDiscoveryLimits,
    ) -> Result<Vec<PathBuf>, std::io::Error> {
        let mut state = SessionDiscoveryState::default();
        self.walk_session_paths(&self.directory, Path::new(""), 0, limits, &mut state)?;
        state.paths.sort_unstable();
        Ok(state.paths)
    }

    /// Walk one retained directory without granting ambient authority or following links.
    fn walk_session_paths(
        &self,
        directory: &Dir,
        relative_directory: &Path,
        depth: usize,
        limits: SessionDiscoveryLimits,
        state: &mut SessionDiscoveryState,
    ) -> Result<(), std::io::Error> {
        for entry in directory.entries()? {
            let entry = entry?;
            state.entries_seen = state.entries_seen.saturating_add(1);
            if state.entries_seen > limits.max_entries {
                return Err(std::io::Error::other(format!(
                    "retained-root discovery exceeded {} filesystem entries",
                    limits.max_entries
                )));
            }

            let name = entry.file_name();
            let name_path = Path::new(&name);
            let mut components = name_path.components();
            if !matches!(components.next(), Some(Component::Normal(_)))
                || components.next().is_some()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "retained-root discovery returned a non-normal entry name",
                ));
            }
            let relative = relative_directory.join(name_path);
            let relative_bytes = relative.as_os_str().as_encoded_bytes().len();
            if relative_bytes > limits.max_path_bytes {
                return Err(std::io::Error::other(format!(
                    "retained-root discovery path exceeded {} encoded bytes",
                    limits.max_path_bytes
                )));
            }

            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth >= limits.max_depth {
                    return Err(std::io::Error::other(format!(
                        "retained-root discovery exceeded directory depth {}",
                        limits.max_depth
                    )));
                }
                let mut options = CapOpenOptions::new();
                options.read(true);
                options
                    ._cap_fs_ext_follow(FollowSymlinks::No)
                    ._cap_fs_ext_nonblock(true)
                    ._cap_fs_ext_maybe_dir(true);
                let child_file = directory.open_with(name_path, &options)?;
                if !child_file.metadata()?.is_dir() {
                    return Err(std::io::Error::other(format!(
                        "retained-root directory entry changed during discovery: {}",
                        relative.display()
                    )));
                }
                let child = Dir::from_std_file(child_file.into_std());
                self.walk_session_paths(&child, &relative, depth + 1, limits, state)?;
                continue;
            }
            if !file_type.is_file()
                || relative
                    .extension()
                    .and_then(|extension| extension.to_str())
                    != Some("jsonl")
            {
                continue;
            }

            let mut options = CapOpenOptions::new();
            options.read(true);
            options
                ._cap_fs_ext_follow(FollowSymlinks::No)
                ._cap_fs_ext_nonblock(true);
            let file = directory.open_with(name_path, &options)?;
            if !file.metadata()?.is_file() {
                return Err(std::io::Error::other(format!(
                    "retained-root session entry changed during discovery: {}",
                    relative.display()
                )));
            }
            if state.paths.len() >= limits.max_paths {
                return Err(std::io::Error::other(format!(
                    "retained-root discovery exceeded {} session paths",
                    limits.max_paths
                )));
            }
            state.path_bytes = state.path_bytes.saturating_add(relative_bytes);
            if state.path_bytes > limits.max_total_path_bytes {
                return Err(std::io::Error::other(format!(
                    "retained-root discovery exceeded {} aggregate path bytes",
                    limits.max_total_path_bytes
                )));
            }
            state.paths.push(self.configured_prefix.join(relative));
        }
        Ok(())
    }

    /// Convert an event path into a normalized path relative to this retained root.
    fn relative_path(&self, path: &Path) -> Result<PathBuf, std::io::Error> {
        let absolute = absolute_unresolved_path(path)?;
        let relative = absolute
            .strip_prefix(&self.configured_prefix)
            .or_else(|_| absolute.strip_prefix(&self.resolved_prefix))
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "session path resolves outside the supervisor watch root",
                )
            })?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "session path contains an unsafe root-relative component",
            ));
        }
        Ok(relative.to_path_buf())
    }
}

/// Build an absolute path without erasing parent components supplied by an event.
fn absolute_unresolved_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// Build a fully qualified configured root without normalizing caller-supplied parent traversal.
fn absolute_configured_path(path: &Path) -> Result<PathBuf, std::io::Error> {
    let absolute = absolute_unresolved_path(path)?;
    if !absolute.is_absolute()
        || absolute
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "supervisor watch root must be fully qualified without parent components",
        ));
    }
    Ok(absolute)
}

/// Read the stable identity of a retained directory capability.
fn directory_identity(directory: &Dir) -> Result<FileIdentity, std::io::Error> {
    file_identity(&directory.try_clone()?.into_std_file())
}

/// Retain a cloned Unix handle so inode identity cannot be recycled while its offset is cached.
#[cfg(unix)]
fn file_identity(file: &File) -> Result<FileIdentity, std::io::Error> {
    Ok(FileIdentity(same_file::Handle::from_file(
        file.try_clone()?,
    )?))
}

/// Read and retain the complete Windows file identity, including ReFS's 128-bit identifier.
#[cfg(windows)]
fn file_identity(file: &File) -> Result<FileIdentity, std::io::Error> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };

    let retained = file.try_clone()?;
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `retained` owns a valid handle and `info` is writable for exactly the supplied size.
    let succeeded = unsafe {
        GetFileInformationByHandleEx(
            retained.as_raw_handle(),
            FileIdInfo,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if succeeded == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(FileIdentity {
        _retained: retained,
        volume_serial_number: info.VolumeSerialNumber,
        file_id: info.FileId.Identifier,
    })
}

/// Produce a unique identity on unsupported platforms so every scan restarts safely.
#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File) -> Result<FileIdentity, std::io::Error> {
    Ok(FileIdentity(
        NEXT_FALLBACK_FILE_IDENTITY.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Open one regular session file through the retained directory capability.
fn open_confined_session_file(
    path: &Path,
    watch_root: &WatchRoot,
) -> Result<(PathBuf, File, FileIdentity), std::io::Error> {
    let relative = watch_root.relative_path(path)?;
    let mut options = CapOpenOptions::new();
    options.read(true);
    // cap-primitives exposes these wrappers for cap-fs-ext; direct use keeps the capability
    // resolver's policy explicit without adding a trait-only dependency.
    options
        ._cap_fs_ext_follow(FollowSymlinks::No)
        ._cap_fs_ext_nonblock(true);
    let file = watch_root
        .directory
        .open_with(&relative, &options)?
        .into_std();
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "opened session path is not a regular file",
        ));
    }
    let identity = file_identity(&file)?;
    Ok((relative, file, identity))
}

/// Consume one JSONL record with fixed memory and byte budgets.
fn read_session_line<R: BufRead>(
    reader: &mut R,
    initially_discarding: bool,
    budget: usize,
) -> Result<Option<SessionLineRead>, std::io::Error> {
    if budget == 0 {
        return Ok(None);
    }
    let mut bytes = (!initially_discarding).then(|| Vec::with_capacity(8192));
    let mut consumed = 0;
    let mut discarding = initially_discarding;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if consumed == 0 {
                Ok(None)
            } else {
                Ok(Some(SessionLineRead {
                    bytes,
                    consumed,
                    terminated: false,
                    discarding,
                }))
            };
        }

        let allowed = available.len().min(budget - consumed);
        let segment = &available[..allowed];
        let newline = segment.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(allowed, |index| index + 1);
        let content_length = newline.map_or(take, |_| take - 1);

        if let Some(line) = bytes.as_mut() {
            if line.len().saturating_add(content_length) > MAX_SESSION_LINE_BYTES {
                bytes = None;
                discarding = true;
            } else {
                line.extend_from_slice(&segment[..content_length]);
            }
        }

        reader.consume(take);
        consumed += take;
        if newline.is_some() {
            return Ok(Some(SessionLineRead {
                bytes,
                consumed,
                terminated: true,
                discarding: false,
            }));
        }
        if consumed == budget {
            return Ok(Some(SessionLineRead {
                bytes,
                consumed,
                terminated: false,
                discarding,
            }));
        }
    }
}

/// Read one bounded batch from a session file and commit only complete-record offsets.
fn read_new_entries(
    path: &Path,
    watch_root: &WatchRoot,
    positions: &mut LruCache<PathBuf, ReadPosition>,
) -> Result<ReadBatch, std::io::Error> {
    let (cache_key, file, identity) = open_confined_session_file(path, watch_root)?;
    let file_len = file.metadata()?.len();
    let cached = positions.peek(&cache_key);
    let same_file = cached
        .map(|position| position.identity.eq(&identity))
        .unwrap_or(false);
    let (start_pos, mut discarding) = match cached {
        Some(position) if same_file && file_len >= position.offset => {
            (position.offset, position.discarding_oversized_line)
        }
        _ => (0, false),
    };

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(start_pos))?;

    let mut committed_pos = start_pos;
    let mut remaining_budget = MAX_SESSION_SCAN_BYTES;
    let mut examined_records = 0;
    let mut entries = Vec::new();
    let mut oversized_records = 0;
    let mut awaiting_termination = false;

    while remaining_budget > 0 && examined_records < MAX_SESSION_SCAN_RECORDS {
        if !discarding && remaining_budget <= MAX_SESSION_LINE_BYTES {
            break;
        }
        let was_discarding = discarding;
        let Some(line) = read_session_line(&mut reader, discarding, remaining_budget)? else {
            break;
        };
        if line.consumed == 0 {
            break;
        }
        remaining_budget -= line.consumed;

        if line.bytes.is_none() {
            committed_pos = committed_pos.saturating_add(line.consumed as u64);
            discarding = line.discarding;
            if !was_discarding {
                oversized_records += 1;
                tracing::warn!(
                    path = %path.display(),
                    max_bytes = MAX_SESSION_LINE_BYTES,
                    "supervisor rejected an oversized session record"
                );
            }
            if line.terminated {
                examined_records += 1;
            }
            continue;
        }

        if !line.terminated {
            awaiting_termination = true;
            break;
        }

        committed_pos = committed_pos.saturating_add(line.consumed as u64);
        discarding = false;
        examined_records += 1;
        let bytes = line
            .bytes
            .expect("bounded record bytes are present after the oversized branch");
        if bytes.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let is_tool = value
                .as_object()
                .map(|object| {
                    object.contains_key("tool_name")
                        || object.get("type").and_then(|item| item.as_str()) == Some("tool_use")
                })
                .unwrap_or(false);
            if is_tool {
                entries.push(value);
            }
        }
    }

    let current_len = reader.get_ref().metadata()?.len();
    let has_more = !awaiting_termination && current_len > committed_pos;
    positions.put(
        cache_key,
        ReadPosition {
            identity,
            offset: committed_pos,
            discarding_oversized_line: discarding,
        },
    );
    Ok(ReadBatch {
        entries,
        oversized_records,
        has_more,
        committed_offset: committed_pos,
        observed_len: current_len,
    })
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
        supervisor_for_watch_dir(std::env::temp_dir(), rules, allowed_paths)
    }

    /// Build a supervisor rooted at an explicit watch directory.
    fn supervisor_for_watch_dir(
        watch_dir: PathBuf,
        rules: Vec<Rule>,
        allowed_paths: Vec<String>,
    ) -> (Supervisor, Arc<AxonBus>) {
        let bus = Arc::new(AxonBus::new());
        let sup = Supervisor::new(
            SupervisorConfig {
                watch_dir,
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

    /// Continuous-write debouncer events enter the same bounded scan queue as settled events.
    #[test]
    fn continuous_watcher_events_are_forwarded() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let path = PathBuf::from("continuous.jsonl");
        let watcher_failed = AtomicBool::new(false);
        forward_watcher_events(
            &tx,
            &watcher_failed,
            Ok(vec![notify_debouncer_mini::DebouncedEvent::new(
                path.clone(),
                notify_debouncer_mini::DebouncedEventKind::AnyContinuous,
            )]),
        );

        assert_eq!(rx.blocking_recv(), Some(path));
        assert!(!watcher_failed.load(Ordering::Acquire));
    }

    /// A watcher backend error latches an unhealthy state even when no path can be delivered.
    #[test]
    fn watcher_backend_errors_are_latched() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let watcher_failed = AtomicBool::new(false);
        forward_watcher_events(
            &tx,
            &watcher_failed,
            Err(notify::Error::generic("test watcher failure")),
        );

        assert!(watcher_failed.load(Ordering::Acquire));
    }

    /// A saturated accelerator queue latches failure without blocking the watcher callback.
    #[test]
    fn watcher_queue_saturation_is_latched_without_blocking() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let retained = PathBuf::from("retained.jsonl");
        tx.try_send(retained.clone()).expect("fill watcher queue");
        let watcher_failed = AtomicBool::new(false);

        forward_watcher_events(
            &tx,
            &watcher_failed,
            Ok(vec![notify_debouncer_mini::DebouncedEvent::new(
                PathBuf::from("dropped.jsonl"),
                notify_debouncer_mini::DebouncedEventKind::Any,
            )]),
        );

        assert!(watcher_failed.load(Ordering::Acquire));
        assert_eq!(rx.try_recv(), Ok(retained));
        assert!(rx.try_recv().is_err());
    }

    /// A ready continuation runs after one watcher event even when more events are buffered.
    #[tokio::test]
    async fn continuation_queue_alternates_with_watcher_events() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let first_event = PathBuf::from("first.jsonl");
        let second_event = PathBuf::from("second.jsonl");
        let continuation = PathBuf::from("continuation.jsonl");
        tx.send(first_event.clone())
            .await
            .expect("queue first event");
        tx.send(second_event).await.expect("queue second event");

        let mut pending = VecDeque::from([continuation.clone()]);
        let mut pending_paths = HashSet::from([continuation.clone()]);
        let mut prefer_pending = false;
        assert_eq!(
            next_scan_path(
                &mut rx,
                &mut pending,
                &mut pending_paths,
                &mut prefer_pending,
            )
            .await,
            Some((first_event, false))
        );
        assert_eq!(
            next_scan_path(
                &mut rx,
                &mut pending,
                &mut pending_paths,
                &mut prefer_pending,
            )
            .await,
            Some((continuation, true))
        );
        assert!(pending_paths.is_empty());
    }

    /// A full continuation queue still alternates with buffered watcher events.
    #[tokio::test]
    async fn full_continuation_queue_does_not_starve_buffered_event() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let event = PathBuf::from("event.jsonl");
        tx.send(event.clone()).await.expect("queue event");
        let mut pending = (0..MAX_PENDING_SCAN_PATHS)
            .map(|index| PathBuf::from(format!("pending-{index}.jsonl")))
            .collect::<VecDeque<_>>();
        let expected = pending.front().cloned().expect("pending path");
        let mut pending_paths = pending.iter().cloned().collect::<HashSet<_>>();
        let mut prefer_pending = false;

        assert_eq!(
            next_scan_path(
                &mut rx,
                &mut pending,
                &mut pending_paths,
                &mut prefer_pending,
            )
            .await,
            Some((event, false))
        );
        assert_eq!(
            next_scan_path(
                &mut rx,
                &mut pending,
                &mut pending_paths,
                &mut prefer_pending,
            )
            .await,
            Some((expected.clone(), true))
        );
        assert!(!pending_paths.contains(&expected));
        assert_eq!(pending.len(), MAX_PENDING_SCAN_PATHS - 1);
    }

    /// Continuation overflow emits a critical violation without stopping the supervisor.
    #[tokio::test]
    async fn continuation_overflow_emits_violation_and_preserves_queue() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let mut pending = (0..MAX_PENDING_SCAN_PATHS)
            .map(|index| PathBuf::from(format!("pending-{index}.jsonl")))
            .collect::<VecDeque<_>>();
        let mut pending_paths = pending.iter().cloned().collect::<HashSet<_>>();

        assert_eq!(
            sup.enqueue_continuation(
                PathBuf::from("overflow.jsonl"),
                &mut pending,
                &mut pending_paths,
            ),
            1
        );
        assert_eq!(pending.len(), MAX_PENDING_SCAN_PATHS);
        assert!(!pending_paths.contains(Path::new("overflow.jsonl")));
        let event = rx.recv().await.expect("overload violation");
        assert_eq!(event.rule_id, SUPERVISOR_OVERLOAD_RULE_ID);
        assert_eq!(event.severity, "critical");
    }

    /// A session scan error becomes a typed critical coverage event for subscribers.
    #[tokio::test]
    async fn scan_failure_publishes_critical_coverage_event() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = Path::new("failed.jsonl");

        sup.publish_scan_failure(path, "permission denied".to_string());

        let event = rx.recv().await.expect("scan failure event");
        assert_eq!(event.rule_id, SUPERVISOR_SCAN_FAILURE_RULE_ID);
        assert_eq!(event.severity, "critical");
        assert!(event.context.contains("failed.jsonl"));
        assert!(event.context.contains("permission denied"));
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

    /// A direct scan cannot read a session file outside the configured watch root.
    #[test]
    fn scan_rejects_path_outside_watch_root() {
        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let outside = temp_path("jsonl");
        append_bash(&outside, "ls", "outside");
        let (mut sup, _bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());

        let error = sup
            .scan_file(&outside)
            .expect_err("outside session path must be rejected");
        assert!(error.to_string().contains("outside"));

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// A symbolic-link leaf is rejected even when its target is inside the watch root.
    #[cfg(unix)]
    #[test]
    fn scan_rejects_symbolic_link_leaf() {
        use std::os::unix::fs::symlink;

        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let target = watch_dir.join("target.jsonl");
        let link = watch_dir.join("linked.jsonl");
        append_bash(&target, "ls", "linked");
        symlink(&target, &link).expect("create session symlink");
        let (mut sup, _bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());

        sup.scan_file(&link)
            .expect_err("symbolic-link session path must be rejected");

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// A parent-directory symbolic link cannot escape the retained watch capability.
    #[cfg(unix)]
    #[test]
    fn scan_rejects_symbolic_link_parent_escape() {
        use std::os::unix::fs::symlink;

        let watch_dir = temp_path("watch-root");
        let outside_dir = temp_path("outside-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        std::fs::create_dir(&outside_dir).expect("create outside root");
        let outside = outside_dir.join("outside.jsonl");
        append_bash(&outside, "git push --force", "outside");
        let linked_parent = watch_dir.join("linked");
        symlink(&outside_dir, &linked_parent).expect("create parent symlink");
        let (mut sup, _bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());

        sup.scan_file(&linked_parent.join("outside.jsonl"))
            .expect_err("parent symlink escape must be rejected");

        drop(sup);
        let _ = std::fs::remove_file(&linked_parent);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir(&outside_dir);
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// A raw event path containing a parent component is rejected before normalization.
    #[test]
    fn scan_rejects_parent_components_inside_watch_root() {
        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let path = watch_dir.join("nested").join("..").join("session.jsonl");
        append_bash(
            &watch_dir.join("session.jsonl"),
            "git push --force",
            "unsafe",
        );
        let (mut sup, _bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());

        sup.scan_file(&path)
            .expect_err("parent-bearing event path must be rejected");

        drop(sup);
        let _ = std::fs::remove_file(watch_dir.join("session.jsonl"));
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// A configured watch root containing parent traversal is rejected instead of normalized.
    #[test]
    fn watch_root_rejects_parent_components() {
        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let configured = watch_dir.join("nested").join("..");

        WatchRoot::open(&configured)
            .err()
            .expect("parent-bearing watch root must be rejected");

        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// Retained-root discovery recursively returns only regular JSONL files in stable order.
    #[test]
    fn retained_root_discovery_finds_nested_jsonl_files() {
        let watch_dir = temp_path("watch-root");
        let nested = watch_dir.join("nested");
        std::fs::create_dir_all(&nested).expect("create nested watch root");
        append_bash(&watch_dir.join("root.jsonl"), "ls", "root");
        append_bash(&nested.join("nested.jsonl"), "ls", "nested");
        std::fs::write(watch_dir.join("ignored.txt"), b"not a session").expect("write ignored");
        let root = WatchRoot::open(&watch_dir).expect("open watch root");

        let paths = root
            .discover_session_paths(SESSION_DISCOVERY_LIMITS)
            .expect("discover session files");
        assert_eq!(
            paths,
            vec![
                root.configured_prefix.join("nested").join("nested.jsonl"),
                root.configured_prefix.join("root.jsonl"),
            ]
        );

        drop(root);
        let _ = std::fs::remove_dir_all(&watch_dir);
    }

    /// Retained-root discovery skips symbolic-link files and directories without following them.
    #[cfg(unix)]
    #[test]
    fn retained_root_discovery_skips_symbolic_links() {
        use std::os::unix::fs::symlink;

        let watch_dir = temp_path("watch-root");
        let outside_dir = temp_path("outside-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        std::fs::create_dir(&outside_dir).expect("create outside root");
        let regular = watch_dir.join("regular.jsonl");
        let outside = outside_dir.join("outside.jsonl");
        append_bash(&regular, "ls", "regular");
        append_bash(&outside, "git push --force", "outside");
        symlink(&outside, watch_dir.join("linked.jsonl")).expect("link session file");
        symlink(&outside_dir, watch_dir.join("linked-dir")).expect("link session directory");
        let root = WatchRoot::open(&watch_dir).expect("open watch root");

        let paths = root
            .discover_session_paths(SESSION_DISCOVERY_LIMITS)
            .expect("discover session files");
        assert_eq!(paths, vec![root.configured_prefix.join("regular.jsonl")]);

        drop(root);
        let _ = std::fs::remove_dir_all(&watch_dir);
        let _ = std::fs::remove_dir_all(&outside_dir);
    }

    /// Retained-root discovery rejects traversal that exceeds an explicit entry ceiling.
    #[test]
    fn retained_root_discovery_rejects_entry_overflow() {
        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        append_bash(&watch_dir.join("first.jsonl"), "ls", "first");
        append_bash(&watch_dir.join("second.jsonl"), "ls", "second");
        let root = WatchRoot::open(&watch_dir).expect("open watch root");
        let limits = SessionDiscoveryLimits {
            max_entries: 1,
            ..SESSION_DISCOVERY_LIMITS
        };

        let error = root
            .discover_session_paths(limits)
            .expect_err("entry overflow must reject discovery");
        assert!(error.to_string().contains("filesystem entries"));

        drop(root);
        let _ = std::fs::remove_dir_all(&watch_dir);
    }

    /// A Windows rooted, drive, UNC, or device event path cannot become a relative capability.
    #[cfg(windows)]
    #[test]
    fn windows_absolute_path_forms_cannot_escape_watch_root() {
        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let root = WatchRoot::open(&watch_dir).expect("open watch root");

        for path in [
            Path::new(r"\outside.jsonl"),
            Path::new(r"C:\outside.jsonl"),
            Path::new(r"\\server\share\outside.jsonl"),
            Path::new(r"\\?\C:\outside.jsonl"),
        ] {
            root.relative_path(path)
                .expect_err("foreign Windows path form must be rejected");
        }

        drop(root);
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// A Windows file reparse point is rejected without reading its target.
    #[cfg(windows)]
    #[test]
    fn scan_rejects_windows_symbolic_link_leaf() {
        use std::os::windows::fs::symlink_file;

        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let target = watch_dir.join("target.jsonl");
        let link = watch_dir.join("linked.jsonl");
        append_bash(&target, "git push --force", "linked");
        symlink_file(&target, &link).expect("create session symlink");
        let (mut sup, _bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());

        sup.scan_file(&link)
            .expect_err("Windows symbolic-link session path must be rejected");

        drop(sup);
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// A Unix FIFO is rejected through a nonblocking capability-relative open.
    #[cfg(unix)]
    #[test]
    fn scan_rejects_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let fifo = watch_dir.join("session.jsonl");
        let encoded = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path has no NUL");
        // SAFETY: `encoded` is a live NUL-terminated path and the mode is valid.
        let result = unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) };
        assert_eq!(
            result,
            0,
            "create FIFO: {}",
            std::io::Error::last_os_error()
        );
        let (mut sup, _bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());

        sup.scan_file(&fifo)
            .expect_err("FIFO session path must be rejected");

        drop(sup);
        let _ = std::fs::remove_file(&fifo);
        let _ = std::fs::remove_dir(&watch_dir);
    }

    /// Renaming and replacing the configured root never grants authority over the replacement.
    #[cfg(unix)]
    #[tokio::test]
    async fn retained_watch_root_does_not_follow_replacement() {
        let watch_dir = temp_path("watch-root");
        let retained_dir = temp_path("retained-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let path = watch_dir.join("session.jsonl");
        append_bash(&path, "ls", "initial");
        let (mut sup, bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        assert_eq!(sup.scan_file(&path).expect("initial scan"), 0);

        std::fs::rename(&watch_dir, &retained_dir).expect("rename retained root");
        std::fs::create_dir(&watch_dir).expect("create replacement root");
        assert!(
            !sup.watch_root
                .as_ref()
                .expect("retained root")
                .matches_current_path(&watch_dir)
                .expect("compare root identity"),
            "replacement path must not match the retained root"
        );
        append_bash(
            &retained_dir.join("session.jsonl"),
            "git push --force",
            "retained",
        );
        append_bash(&path, "ls replacement", "replacement");

        assert_eq!(sup.scan_file(&path).expect("scan retained root"), 1);
        let event = rx.recv().await.expect("retained-root violation");
        assert_eq!(event.rule_id, "no-force-push");
        assert_eq!(event.session_id.as_deref(), Some("retained"));

        drop(sup);
        let _ = std::fs::remove_dir_all(&watch_dir);
        let _ = std::fs::remove_dir_all(&retained_dir);
    }

    /// Replacing one session pathname resets its cursor even when the new file is not shorter.
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn same_name_file_replacement_resets_cursor() {
        let watch_dir = temp_path("watch-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
        let path = watch_dir.join("session.jsonl");
        let old_path = watch_dir.join("old-session.jsonl");
        append_bash(&path, "ls", "old");
        let (mut sup, bus) =
            supervisor_for_watch_dir(watch_dir.clone(), default_rules(), Vec::new());
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        assert_eq!(sup.scan_file(&path).expect("scan old file"), 0);

        std::fs::rename(&path, &old_path).expect("retain old file");
        append_bash(
            &path,
            "git push --force && echo padding-padding-padding-padding",
            "new",
        );
        assert_eq!(sup.scan_file(&path).expect("scan replacement"), 1);
        let event = rx.recv().await.expect("replacement violation");
        assert_eq!(event.rule_id, "no-force-push");
        assert_eq!(event.session_id.as_deref(), Some("new"));

        drop(sup);
        let _ = std::fs::remove_dir_all(&watch_dir);
    }

    /// An oversized record fails closed and does not hide a later detector violation.
    #[tokio::test]
    async fn oversized_record_does_not_hide_following_violation() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = temp_path("jsonl");
        let mut file = std::fs::File::create(&path).expect("create session");
        file.write_all(&vec![b'x'; MAX_SESSION_LINE_BYTES + 1])
            .expect("write oversized record");
        file.write_all(b"\n").expect("terminate oversized record");
        drop(file);
        append_bash(&path, "git push --force", "after-oversized");

        assert_eq!(sup.scan_file(&path).expect("scan"), 2);
        let oversized = rx.recv().await.expect("oversized-record violation");
        assert_eq!(oversized.rule_id, OVERSIZED_SESSION_RECORD_RULE_ID);
        assert_eq!(oversized.severity, "critical");
        let detected = rx.recv().await.expect("following violation");
        assert_eq!(detected.rule_id, "no-force-push");
        assert_eq!(detected.session_id.as_deref(), Some("after-oversized"));

        let _ = std::fs::remove_file(&path);
    }

    /// A direct scan drains every complete record in its initial snapshot across batch limits.
    #[tokio::test]
    async fn direct_scan_drains_initial_snapshot_backlog() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = temp_path("jsonl");
        let mut file = std::fs::File::create(&path).expect("create session");
        for index in 0..MAX_SESSION_SCAN_RECORDS {
            writeln!(file, "{{\"type\":\"assistant\",\"index\":{index}}}")
                .expect("write non-tool record");
        }
        drop(file);
        append_bash(&path, "git push --force", "after-batch-limit");

        assert_eq!(sup.scan_file(&path).expect("drain snapshot"), 1);
        let event = rx.recv().await.expect("backlog violation");
        assert_eq!(event.rule_id, "no-force-push");
        assert_eq!(event.session_id.as_deref(), Some("after-batch-limit"));

        let _ = std::fs::remove_file(&path);
    }

    /// An unterminated JSONL record is not committed until its newline arrives.
    #[tokio::test]
    async fn unterminated_record_waits_for_newline() {
        let (mut sup, bus) = supervisor();
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let path = temp_path("jsonl");
        let encoded = serde_json::to_vec(&serde_json::json!({
            "tool_name": "Bash",
            "tool_input": { "command": "git push --force" },
            "sessionId": "unterminated",
        }))
        .expect("encode session record");
        std::fs::write(&path, encoded).expect("write unterminated record");
        assert_eq!(sup.scan_file(&path).expect("scan partial record"), 0);

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("reopen session");
        file.write_all(b"\n").expect("terminate session record");
        drop(file);
        assert_eq!(sup.scan_file(&path).expect("scan complete record"), 1);
        let event = rx.recv().await.expect("completed violation");
        assert_eq!(event.session_id.as_deref(), Some("unterminated"));

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
        let allowed_root = temp_path("scope-root");
        std::fs::create_dir(&allowed_root).expect("create allowed root");
        let allowed_target = allowed_root.join("README.md");
        let (mut sup, bus) = supervisor_with(
            default_rules(),
            vec![allowed_root.to_string_lossy().into_owned()],
        );
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
                "tool_input": { "file_path": allowed_target },
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
        let _ = std::fs::remove_dir(&allowed_root);
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

    /// The startup retained-root discovery scans a session file created before watcher startup.
    #[tokio::test(flavor = "multi_thread")]
    async fn startup_discovery_scans_preexisting_session_file() {
        let watch_dir = std::env::temp_dir().join(format!(
            "eidolon-supervisor-existing-{}",
            syntheos_contracts::EventId::new()
        ));
        std::fs::create_dir(&watch_dir).expect("create watch root");
        append_bash(
            &watch_dir.join("existing.jsonl"),
            "git push --force",
            "existing",
        );
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
            bus,
        )
        .expect("valid config");
        let task = tokio::spawn(sup.run());

        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("startup discovery must publish within 10s")
            .expect("startup discovery event");
        assert_eq!(event.rule_id, "no-force-push");
        assert_eq!(event.session_id.as_deref(), Some("existing"));

        task.abort();
        let _ = std::fs::remove_dir_all(&watch_dir);
    }

    /// Periodic retained-root discovery finds new files without the watcher accelerator.
    #[tokio::test(flavor = "multi_thread")]
    async fn periodic_discovery_is_authoritative_without_watcher() {
        let watch_dir = std::env::temp_dir().join(format!(
            "eidolon-supervisor-periodic-{}",
            syntheos_contracts::EventId::new()
        ));
        std::fs::create_dir(&watch_dir).expect("create watch root");
        append_bash(
            &watch_dir.join("readiness.jsonl"),
            "periodic-probe",
            "readiness",
        );
        let bus = Arc::new(AxonBus::new());
        let mut rx = bus.subscribe_typed::<ViolationDetected>();
        let sup = Supervisor::new(
            SupervisorConfig {
                watch_dir: watch_dir.clone(),
                rules: vec![Rule {
                    id: "periodic-probe".to_string(),
                    check_type: CheckType::RuleMatch,
                    pattern: "periodic-probe".to_string(),
                    severity: Severity::Critical,
                    cooldown_secs: 0,
                    message: "periodic discovery probe".to_string(),
                }],
                allowed_paths: Vec::new(),
                tenant: TenantId::new(),
                principal: PrincipalId::new(),
            },
            bus,
        )
        .expect("valid config");
        let task = tokio::spawn(sup.run_loop(false, Duration::from_millis(50)));

        let readiness = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("startup discovery must publish")
            .expect("startup discovery event");
        assert_eq!(readiness.session_id.as_deref(), Some("readiness"));
        append_bash(
            &watch_dir.join("periodic.jsonl"),
            "periodic-probe",
            "periodic",
        );
        let periodic = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("periodic discovery must publish")
            .expect("periodic discovery event");
        assert_eq!(periodic.session_id.as_deref(), Some("periodic"));

        task.abort();
        let _ = std::fs::remove_dir_all(&watch_dir);
    }

    /// End-to-end: the event accelerator sees a new session file and publishes the violation.
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

        let path = watch_dir.join("session-1.jsonl");
        let readiness_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            append_bash(&path, "git push --force", "live-1");
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some(event)) => {
                    assert_eq!(event.rule_id, "no-force-push");
                    assert_eq!(event.session_id.as_deref(), Some("live-1"));
                    break;
                }
                Ok(None) => panic!("watcher event channel closed before readiness"),
                Err(_) => {
                    assert!(
                        Instant::now() < readiness_deadline,
                        "watcher did not publish within 10s"
                    );
                }
            }
        }

        task.abort();
        let _ = std::fs::remove_dir_all(&watch_dir);
    }

    /// Replacing a live watch root emits a critical coverage violation and stops the watcher.
    #[cfg(unix)]
    #[tokio::test]
    async fn watcher_stops_when_live_root_is_replaced() {
        let watch_dir = temp_path("watch-root");
        let retained_dir = temp_path("retained-root");
        std::fs::create_dir(&watch_dir).expect("create watch root");
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
            bus,
        )
        .expect("valid config");
        let task = tokio::spawn(sup.run());

        let readiness_path = watch_dir.join("readiness.jsonl");
        let readiness_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            append_bash(&readiness_path, "git push --force", "readiness");
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some(event)) => {
                    assert_eq!(event.rule_id, "no-force-push");
                    assert_eq!(event.session_id.as_deref(), Some("readiness"));
                    break;
                }
                Ok(None) => panic!("watcher event channel closed before readiness"),
                Err(_) => {
                    assert!(
                        Instant::now() < readiness_deadline,
                        "watcher readiness was not confirmed within 10s"
                    );
                }
            }
        }
        std::fs::rename(&watch_dir, &retained_dir).expect("rename retained root");
        std::fs::create_dir(&watch_dir).expect("create replacement root");
        std::fs::write(
            retained_dir.join("trigger.txt"),
            b"trigger watcher callback",
        )
        .expect("write retained-root trigger");

        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("root replacement must publish within 10s")
            .expect("root replacement event");
        assert_eq!(event.rule_id, WATCH_ROOT_IDENTITY_LOST_RULE_ID);
        assert_eq!(event.severity, "critical");
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("watcher must stop within 10s")
            .expect("watcher task must finish cleanly");

        let _ = std::fs::remove_dir_all(&watch_dir);
        let _ = std::fs::remove_dir_all(&retained_dir);
    }
}
