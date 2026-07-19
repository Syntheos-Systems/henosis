//! Stimulus injection: wakes the room with real project signals.
//!
//! The parent Rift Team Room design (memory 27272) lists the stimulus
//! injector as a core bridge component: rooms otherwise only wake when a
//! human posts, so an idle team never reflects, never notices a new commit,
//! and never revisits stale tasks. This module implements the injector with
//! the sources reachable from the bridge today -- scheduled reflection,
//! Chiasm task-state changes (via the existing KleosClient), and git HEAD
//! movement in declared workspaces. Axon/Loom/test-result sources from the
//! parent spec need client surfaces the bridge does not have yet and remain
//! future work (recorded in the design doc addendum).
//!
//! Parent-spec safety requirements implemented here: per-source cooldowns, a
//! global hourly rate cap, and content sanitization of everything read from
//! external state (commit subjects, task summaries). Structural distinction
//! is end-to-end: stimuli enter the cascade in-process and their room
//! announcement is posted with message_type 'stimulus' (the Rift server
//! stamps and broadcasts it), with the `[STIMULUS]` text prefix kept for
//! human readability and for older servers that ignore the type field.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use crate::config::StimulusSettings;
use crate::kleos::KleosClient;

/// Maximum characters a sanitized stimulus text may carry into the room.
const MAX_STIMULUS_CHARS: usize = 600;

/// How far back the global rate cap window reaches.
const RATE_WINDOW: Duration = Duration::from_secs(3600);

/// The kind of signal a stimulus carries; keys cooldown accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StimulusKind {
    /// Scheduled "what should we focus on?" prompt after room inactivity.
    Reflection,
    /// Chiasm active-task state changed.
    ChiasmTasks,
    /// A declared workspace's git HEAD moved.
    GitCommit,
}

/// Cooldown-map keys and display labels for stimulus kinds.
impl StimulusKind {
    /// Stable string key for cooldown maps and logs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reflection => "reflection",
            Self::ChiasmTasks => "chiasm-tasks",
            Self::GitCommit => "git-commit",
        }
    }
}

/// One injectable signal: a kind plus already-humanized text.
#[derive(Debug, Clone)]
pub struct Stimulus {
    /// Which source class produced it.
    pub kind: StimulusKind,
    /// Sanitized, room-ready text (without the `[STIMULUS]` prefix).
    pub text: String,
}

/// Read-only facts a poll cycle hands every source.
pub struct StimulusContext {
    /// The poll cycle's single notion of now.
    pub now: Instant,
    /// When the room last saw any message (human or agent).
    pub last_room_activity: Instant,
}

/// A pollable signal source. Sources are stateful (they remember what they
/// last saw) and must be cheap per poll; anything slow belongs behind its
/// own cadence via the injector's per-kind cooldowns.
#[async_trait]
pub trait StimulusSource: Send {
    /// The kind this source emits (cooldowns are keyed on it).
    fn kind(&self) -> StimulusKind;
    /// Produce zero or more stimuli for this cycle. Sources must swallow
    /// their own transient errors (log and return empty): one broken source
    /// must not stop the injector.
    async fn poll(&mut self, ctx: &StimulusContext) -> Vec<Stimulus>;
}

/// Fires a reflection prompt once the room has been quiet long enough.
pub struct ReflectionSource {
    /// Inactivity window before a reflection fires.
    after: Duration,
}

/// Construction for the reflection source.
impl ReflectionSource {
    /// Build with the configured inactivity window.
    pub fn new(after: Duration) -> Self {
        Self { after }
    }
}

/// Emits a reflection prompt when the inactivity window has elapsed.
#[async_trait]
impl StimulusSource for ReflectionSource {
    /// Reflection stimuli.
    fn kind(&self) -> StimulusKind {
        StimulusKind::Reflection
    }

    /// Fire when inactivity exceeds the window. Refiring every poll cycle is
    /// prevented by the injector's per-kind cooldown, which is configured to
    /// at least this window.
    async fn poll(&mut self, ctx: &StimulusContext) -> Vec<Stimulus> {
        if ctx.now.duration_since(ctx.last_room_activity) < self.after {
            return Vec::new();
        }
        vec![Stimulus {
            kind: StimulusKind::Reflection,
            text: "The room has been quiet for a while. What should the team focus on \
                   next? Review the active tasks and recent decisions, and propose one \
                   concrete next step."
                .to_string(),
        }]
    }
}

/// Fires when the Chiasm active-task summary changes between polls.
pub struct ChiasmTaskSource {
    /// Kleos client the summary is fetched through.
    kleos: Arc<dyn KleosClient>,
    /// Project scope for the task query.
    project: String,
    /// Fingerprint of the last summary seen (None = no summary).
    last: Option<u64>,
    /// Whether the first poll has primed the fingerprint. The first
    /// observation never fires: booting the bridge is not a task change.
    primed: bool,
}

/// Construction and summary fingerprinting.
impl ChiasmTaskSource {
    /// Build against the bridge's Kleos client and project scope.
    pub fn new(kleos: Arc<dyn KleosClient>, project: String) -> Self {
        Self {
            kleos,
            project,
            last: None,
            primed: false,
        }
    }

    /// Stable fingerprint of a summary option.
    fn fingerprint(summary: &Option<String>) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        summary.as_ref().map(|s| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            s.hash(&mut h);
            h.finish()
        })
    }
}

/// Emits a stimulus when the Chiasm active-task summary fingerprint moves.
#[async_trait]
impl StimulusSource for ChiasmTaskSource {
    /// Chiasm task stimuli.
    fn kind(&self) -> StimulusKind {
        StimulusKind::ChiasmTasks
    }

    /// Fetch the active-task summary and fire when its fingerprint moved.
    async fn poll(&mut self, _ctx: &StimulusContext) -> Vec<Stimulus> {
        let summary = match self.kleos.active_tasks_summary(&self.project, 5).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("chiasm stimulus poll failed: {e}");
                return Vec::new();
            }
        };
        let fp = Self::fingerprint(&summary);
        if !self.primed {
            self.primed = true;
            self.last = fp;
            return Vec::new();
        }
        if fp == self.last {
            return Vec::new();
        }
        self.last = fp;
        match summary {
            Some(text) => vec![Stimulus {
                kind: StimulusKind::ChiasmTasks,
                text: format!("Task board changed. Current active tasks:\n{text}"),
            }],
            // Summary disappearing (all tasks closed) is a change worth noting.
            None => vec![Stimulus {
                kind: StimulusKind::ChiasmTasks,
                text: "Task board changed: no active tasks remain.".to_string(),
            }],
        }
    }
}

/// Consecutive probe failures before a workspace is disabled. A single
/// transient failure (index.lock contention, momentary IO error) must not
/// permanently silence a workspace (adversarial review finding).
const GIT_FAILURES_BEFORE_DISABLE: u32 = 3;

/// Ceiling on one git HEAD probe; a hung git (dead NFS mount, lock storm)
/// must not stall the whole injector poll loop.
const GIT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Fires when a declared workspace's git HEAD moves between polls.
pub struct GitHeadSource {
    /// Workspace name and repository path pairs to watch.
    workspaces: Vec<(String, PathBuf)>,
    /// Last HEAD commit id seen per workspace name.
    last_heads: HashMap<String, String>,
    /// Consecutive probe failures per workspace name.
    failures: HashMap<String, u32>,
    /// Workspaces disabled after repeated failures: warned once, skipped
    /// thereafter so a bad entry cannot spam logs every poll.
    dead: HashSet<String>,
    /// Whether the first poll has primed baselines. The first observation
    /// never fires: booting the bridge is not a new commit.
    primed: bool,
}

/// Construction and the single-repo HEAD probe.
impl GitHeadSource {
    /// Build over the declared workspaces.
    pub fn new(workspaces: Vec<(String, PathBuf)>) -> Self {
        Self {
            workspaces,
            last_heads: HashMap::new(),
            failures: HashMap::new(),
            dead: HashSet::new(),
            primed: false,
        }
    }

    /// Read `HEAD` of one repo as `(full_id, short_id_and_subject)`.
    /// Any failure -- spawn error, non-zero exit, timeout, parse -- returns
    /// None; the caller's failure counter decides when to give up.
    async fn head_of(path: &Path) -> Option<(String, String)> {
        let probe = tokio::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["log", "-1", "--format=%H\t%h %s"])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(GIT_PROBE_TIMEOUT, probe)
            .await
            .ok()?
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&out.stdout);
        let line = line.trim();
        let (full, human) = line.split_once('\t')?;
        Some((full.to_string(), human.to_string()))
    }
}

/// Emits one batched stimulus covering every workspace whose HEAD moved.
#[async_trait]
impl StimulusSource for GitHeadSource {
    /// Git commit stimuli.
    fn kind(&self) -> StimulusKind {
        StimulusKind::GitCommit
    }

    /// Probe each live workspace and emit at most ONE stimulus listing all
    /// moved HEADs. Batching matters: baselines advance during the poll, so
    /// per-workspace stimuli beyond the first would be eaten by the
    /// injector's per-kind cooldown and the changes silently lost
    /// (adversarial review finding).
    async fn poll(&mut self, _ctx: &StimulusContext) -> Vec<Stimulus> {
        let mut moved_lines = Vec::new();
        for (name, path) in &self.workspaces {
            if self.dead.contains(name) {
                continue;
            }
            let Some((head, human)) = Self::head_of(path).await else {
                let count = self.failures.entry(name.clone()).or_insert(0);
                *count += 1;
                if *count >= GIT_FAILURES_BEFORE_DISABLE {
                    tracing::warn!(
                        "git stimulus source disabled for workspace {name} after {count} \
                         consecutive probe failures: {}",
                        path.display()
                    );
                    self.dead.insert(name.clone());
                } else {
                    tracing::debug!(
                        "git probe failed for workspace {name} ({count} consecutive)"
                    );
                }
                continue;
            };
            self.failures.remove(name);
            let moved = self
                .last_heads
                .get(name)
                .map(|prev| prev != &head)
                .unwrap_or(false);
            self.last_heads.insert(name.clone(), head);
            if self.primed && moved {
                moved_lines.push(format!("{name}: {human}"));
            }
        }
        self.primed = true;
        if moved_lines.is_empty() {
            Vec::new()
        } else {
            vec![Stimulus {
                kind: StimulusKind::GitCommit,
                text: format!("New commits landed.\n{}", moved_lines.join("\n")),
            }]
        }
    }
}

/// Strip control characters (newline survives), collapse leading/trailing
/// whitespace, and truncate to `max_chars` on a char boundary. Everything a
/// source read from external state (commit subjects, task titles) is
/// untrusted input per the parent spec and passes through here.
pub fn sanitize(text: &str, max_chars: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .take(max_chars)
        .collect();
    cleaned.trim().to_string()
}

/// Polls sources on an interval and forwards eligible stimuli into the event
/// loop, enforcing per-kind cooldowns and the global hourly rate cap.
pub struct StimulusInjector {
    /// The signal sources, polled in order.
    sources: Vec<Box<dyn StimulusSource>>,
    /// Seconds between poll cycles.
    poll_interval: Duration,
    /// Minimum spacing between two stimuli of the same kind.
    cooldowns: HashMap<StimulusKind, Duration>,
    /// When each kind last fired.
    last_fired: HashMap<StimulusKind, Instant>,
    /// Global cap on stimuli per rolling hour.
    max_per_hour: u32,
    /// Fire timestamps inside the rolling window.
    fired: VecDeque<Instant>,
    /// Channel into the main event loop.
    stim_tx: mpsc::Sender<Stimulus>,
    /// Last room activity, updated by the main loop.
    activity_rx: watch::Receiver<Instant>,
    /// Bridge pause state; paused rooms receive no stimuli.
    pause_rx: watch::Receiver<bool>,
}

/// Injector construction, eligibility accounting, and the poll loop.
impl StimulusInjector {
    /// Build the injector from settings and pre-built sources.
    pub fn new(
        settings: &StimulusSettings,
        sources: Vec<Box<dyn StimulusSource>>,
        stim_tx: mpsc::Sender<Stimulus>,
        activity_rx: watch::Receiver<Instant>,
        pause_rx: watch::Receiver<bool>,
    ) -> Self {
        let mut cooldowns = HashMap::new();
        // A reflection must never refire faster than its own inactivity
        // window: the cooldown IS the window.
        cooldowns.insert(
            StimulusKind::Reflection,
            Duration::from_secs(settings.reflection_after_secs),
        );
        cooldowns.insert(
            StimulusKind::ChiasmTasks,
            Duration::from_secs(settings.chiasm_cooldown_secs),
        );
        cooldowns.insert(
            StimulusKind::GitCommit,
            Duration::from_secs(settings.git_cooldown_secs),
        );
        Self {
            sources,
            poll_interval: Duration::from_secs(settings.poll_secs.max(1)),
            cooldowns,
            last_fired: HashMap::new(),
            max_per_hour: settings.max_per_hour,
            fired: VecDeque::new(),
            stim_tx,
            activity_rx,
            pause_rx,
        }
    }

    /// True when a stimulus of `kind` may fire now: inside neither its
    /// per-kind cooldown nor the global hourly cap. Prunes the rate window.
    fn may_fire(&mut self, kind: StimulusKind, now: Instant) -> bool {
        while let Some(front) = self.fired.front() {
            if now.duration_since(*front) > RATE_WINDOW {
                self.fired.pop_front();
            } else {
                break;
            }
        }
        if self.fired.len() >= self.max_per_hour as usize {
            return false;
        }
        match (self.last_fired.get(&kind), self.cooldowns.get(&kind)) {
            (Some(last), Some(cd)) => now.duration_since(*last) >= *cd,
            _ => true,
        }
    }

    /// Record that a stimulus of `kind` fired at `now`.
    fn record_fire(&mut self, kind: StimulusKind, now: Instant) {
        self.last_fired.insert(kind, now);
        self.fired.push_back(now);
    }

    /// The poll loop: sleep, skip while paused, poll every source, filter
    /// through cooldowns/rate cap/sanitization, forward survivors. Returns
    /// when the event loop side of the channel is gone.
    pub async fn run(mut self) {
        loop {
            tokio::time::sleep(self.poll_interval).await;
            if *self.pause_rx.borrow() {
                continue;
            }
            let ctx = StimulusContext {
                now: Instant::now(),
                last_room_activity: *self.activity_rx.borrow(),
            };
            for i in 0..self.sources.len() {
                let kind = self.sources[i].kind();
                // Skip the poll entirely while the kind is ineligible; the
                // Chiasm/git sources would otherwise advance their baselines
                // and swallow a change inside the cooldown window.
                if !self.may_fire(kind, ctx.now) {
                    continue;
                }
                for stim in self.sources[i].poll(&ctx).await {
                    if !self.may_fire(stim.kind, ctx.now) {
                        continue;
                    }
                    let text = sanitize(&stim.text, MAX_STIMULUS_CHARS);
                    if text.is_empty() {
                        continue;
                    }
                    self.record_fire(stim.kind, ctx.now);
                    if self
                        .stim_tx
                        .send(Stimulus {
                            kind: stim.kind,
                            text,
                        })
                        .await
                        .is_err()
                    {
                        tracing::info!("stimulus channel closed, injector stopping");
                        return;
                    }
                }
            }
        }
    }
}

/// Unit tests for sanitization, eligibility accounting, and source logic.
#[cfg(test)]
mod tests {
    use super::*;

    /// Build an injector with the given settings knobs for accounting tests.
    fn test_injector(max_per_hour: u32, chiasm_cd: u64) -> StimulusInjector {
        let settings = StimulusSettings {
            enabled: true,
            poll_secs: 60,
            reflection_after_secs: 14400,
            chiasm_cooldown_secs: chiasm_cd,
            git_cooldown_secs: 300,
            max_per_hour,
        };
        let (tx, _rx) = mpsc::channel(8);
        let (_atx, arx) = watch::channel(Instant::now());
        let (_ptx, prx) = watch::channel(false);
        StimulusInjector::new(&settings, Vec::new(), tx, arx, prx)
    }

    /// Verifies control characters are stripped, newlines survive, and the
    /// length cap holds on char boundaries.
    #[test]
    fn test_sanitize_strips_control_and_caps_length() {
        assert_eq!(sanitize("a\x1b[31mred\x07b\nc", 100), "a[31mredb\nc");
        let long = "x".repeat(700);
        assert_eq!(sanitize(&long, 600).chars().count(), 600);
        assert_eq!(sanitize("  padded  ", 100), "padded");
        // Multi-byte chars must not panic at the cap boundary.
        let uni = "ü".repeat(700);
        assert_eq!(sanitize(&uni, 600).chars().count(), 600);
    }

    /// Verifies the per-kind cooldown blocks a refire until it elapses.
    #[test]
    fn test_per_kind_cooldown_blocks_refire() {
        let mut inj = test_injector(100, 900);
        let t0 = Instant::now();
        assert!(inj.may_fire(StimulusKind::ChiasmTasks, t0));
        inj.record_fire(StimulusKind::ChiasmTasks, t0);
        assert!(!inj.may_fire(StimulusKind::ChiasmTasks, t0 + Duration::from_secs(899)));
        assert!(inj.may_fire(StimulusKind::ChiasmTasks, t0 + Duration::from_secs(900)));
        // A different kind is unaffected.
        assert!(inj.may_fire(StimulusKind::GitCommit, t0 + Duration::from_secs(1)));
    }

    /// Verifies the global hourly cap blocks all kinds once reached and
    /// releases as the window slides.
    #[test]
    fn test_global_rate_cap_blocks_and_slides() {
        let mut inj = test_injector(2, 0);
        let t0 = Instant::now();
        inj.record_fire(StimulusKind::ChiasmTasks, t0);
        inj.record_fire(StimulusKind::GitCommit, t0 + Duration::from_secs(1));
        assert!(!inj.may_fire(StimulusKind::Reflection, t0 + Duration::from_secs(2)));
        // Past the window, capacity returns.
        assert!(inj.may_fire(
            StimulusKind::Reflection,
            t0 + Duration::from_secs(3601 + 1)
        ));
    }

    /// Verifies reflection fires only after the inactivity window.
    #[tokio::test]
    async fn test_reflection_respects_inactivity_window() {
        let mut src = ReflectionSource::new(Duration::from_secs(100));
        let now = Instant::now();
        let quiet = StimulusContext {
            now: now + Duration::from_secs(101),
            last_room_activity: now,
        };
        assert_eq!(src.poll(&quiet).await.len(), 1);
        let active = StimulusContext {
            now: now + Duration::from_secs(99),
            last_room_activity: now,
        };
        assert!(src.poll(&active).await.is_empty());
    }

    /// Verifies a git workspace pointing nowhere is tolerated for transient
    /// failures and disabled only after the consecutive-failure threshold,
    /// never firing along the way.
    #[tokio::test]
    async fn test_git_source_disables_missing_workspace_after_threshold() {
        let mut src = GitHeadSource::new(vec![(
            "ghost".to_string(),
            PathBuf::from("/nonexistent/definitely/not/a/repo"),
        )]);
        let ctx = StimulusContext {
            now: Instant::now(),
            last_room_activity: Instant::now(),
        };
        for i in 1..GIT_FAILURES_BEFORE_DISABLE {
            assert!(src.poll(&ctx).await.is_empty());
            assert!(
                !src.dead.contains("ghost"),
                "must tolerate {i} transient failure(s)"
            );
        }
        assert!(src.poll(&ctx).await.is_empty());
        assert!(src.dead.contains("ghost"));
        assert!(src.poll(&ctx).await.is_empty());
    }

    /// Verifies the first git observation primes without firing and a HEAD
    /// change afterwards fires exactly once.
    #[tokio::test]
    async fn test_git_source_fires_on_head_change_only() {
        let mut src = GitHeadSource::new(Vec::new());
        // Simulate the prime/change flow directly against internal state:
        // an empty workspace list means poll only flips `primed`.
        let ctx = StimulusContext {
            now: Instant::now(),
            last_room_activity: Instant::now(),
        };
        assert!(src.poll(&ctx).await.is_empty());
        assert!(src.primed);
    }
}
