//! `SessionManager`: owns concurrent agent sessions.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use futures::StreamExt;
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio::task::JoinHandle;

use synapse_provider::Provider;
use synapse_session::SessionStore;
use synapse_tools::ToolRegistry;

use crate::agent::agent_turn_with_pricing;
use crate::context::ConversationContext;
use crate::cost::PricingTable;
use crate::types::AgentConfig;

use super::types::{SessionEvent, SessionId, SessionSnapshot, SessionStatus};

/// Per-session mutable state owned by the manager.
struct SessionState {
    #[allow(dead_code)] // used in later tasks
    label: String,
    #[allow(dead_code)] // used in later tasks
    cwd: PathBuf,
    /// Conversation context shared with the running agent_turn task.
    ctx: Arc<AsyncMutex<ConversationContext>>,
    /// Per-session config template (clone of base with cwd/session_id set).
    config: AgentConfig,
    /// Live snapshot, updated by the running task as events stream.
    snapshot: std::sync::Mutex<SessionSnapshot>,
    /// Handle to the in-flight turn task, if any. Aborting it cancels the turn.
    task: std::sync::Mutex<Option<JoinHandle<()>>>,
}

/// Owns and drives concurrent agent sessions.
pub struct SessionManager {
    sessions: DashMap<SessionId, Arc<SessionState>>,
    next_id: AtomicU64,
    provider: Arc<dyn Provider + Send + Sync>,
    tools: Arc<ToolRegistry>,
    pricing: Arc<PricingTable>,
    store: Option<Arc<SessionStore>>,
    base_config: AgentConfig,
    events_tx: broadcast::Sender<SessionEvent>,
}

impl SessionManager {
    /// Construct a manager. `base_config` is the template each session clones;
    /// per-session fields (`cwd`, `session_id`) are overridden on spawn.
    ///
    /// Events are not buffered for late subscribers: call [`subscribe`] before
    /// [`send`] or the first turn's events will be missed. A receiver that lags
    /// by more than the channel capacity sees `RecvError::Lagged`.
    pub fn new(
        provider: Arc<dyn Provider + Send + Sync>,
        tools: Arc<ToolRegistry>,
        pricing: Arc<PricingTable>,
        store: Option<Arc<SessionStore>>,
        base_config: AgentConfig,
    ) -> Arc<Self> {
        // 2048 buffered events per receiver; a receiver lagging past this sees Lagged.
        let (events_tx, _) = broadcast::channel(2048);
        Arc::new(Self {
            sessions: DashMap::new(),
            next_id: AtomicU64::new(1),
            provider,
            tools,
            pricing,
            store,
            base_config,
            events_tx,
        })
    }

    /// Subscribe to the multiplexed event stream of every session.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events_tx.subscribe()
    }

    /// Create a new idle session rooted at `cwd`. Returns its id.
    ///
    /// If a session store is configured but `create_session` fails, the session
    /// is still created but without persistence (turns are not written); the
    /// failure is logged, not returned.
    pub fn spawn(&self, label: impl Into<String>, cwd: PathBuf) -> SessionId {
        // Relaxed: the id is only a unique key; the sessions.insert below provides
        // the synchronization that publishes the new session to other threads.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let label = label.into();

        // Persist a session row when a store is configured so turns land in SQLite.
        // A store failure degrades to an in-memory-only session (no persistence)
        // rather than failing the spawn; we log it so the degradation is visible.
        let session_id = self.store.as_ref().and_then(|s| {
            let project = cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "synapse".into());
            match s.create_session(&project, &self.base_config.model) {
                Ok(sid) => Some(sid),
                Err(e) => {
                    log::warn!("create_session failed; session {id} runs without persistence: {e}");
                    None
                }
            }
        });

        let mut config = self.base_config.clone();
        config.cwd = cwd.clone();
        config.session_store = self.store.clone();
        config.session_id = session_id;

        let ctx = Arc::new(AsyncMutex::new(ConversationContext::new(
            config.system_prompt.clone(),
            self.tools.all_tool_schemas(),
        )));

        let snapshot = SessionSnapshot {
            id,
            label: label.clone(),
            cwd: cwd.display().to_string(),
            status: SessionStatus::Idle,
            model: config.model.clone(),
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
        };

        let state = Arc::new(SessionState {
            label,
            cwd,
            ctx,
            config,
            snapshot: std::sync::Mutex::new(snapshot),
            task: std::sync::Mutex::new(None),
        });
        self.sessions.insert(id, state);
        id
    }

    /// Current snapshot of one session, if it exists.
    ///
    /// Panics if the snapshot mutex is poisoned (a prior holder panicked).
    pub fn snapshot(&self, id: SessionId) -> Option<SessionSnapshot> {
        // Clone the Arc out so the DashMap shard lock is dropped before we take
        // the snapshot mutex -- keeps the map lock and snapshot lock uncoupled.
        let state = self.sessions.get(&id).map(|e| Arc::clone(e.value()))?;
        let snap = state.snapshot.lock().expect("snapshot lock").clone();
        Some(snap)
    }

    /// Snapshots of all sessions, ordered by id ascending.
    ///
    /// Panics if a snapshot mutex is poisoned.
    pub fn snapshots(&self) -> Vec<SessionSnapshot> {
        // Collect Arc handles first so every DashMap shard lock is released
        // before any snapshot mutex is taken (no map-vs-snapshot lock coupling).
        let states: Vec<Arc<SessionState>> = self
            .sessions
            .iter()
            .map(|e| Arc::clone(e.value()))
            .collect();
        let mut out: Vec<SessionSnapshot> = states
            .iter()
            .map(|s| s.snapshot.lock().expect("snapshot lock").clone())
            .collect();
        // Sort is intentional (stable rail ordering for the renderer), not a bug.
        out.sort_by_key(|s| s.id);
        out
    }

    /// Request cancellation of the in-flight turn for a session, if any.
    ///
    /// `abort()` is asynchronous: it signals the task to stop at its next await
    /// point. We set the snapshot to `Cancelled` immediately, but a task that
    /// was already near completion may still emit a final `TurnEnd` and leave
    /// the status at `Done`. Callers should treat `Cancelled` and a post-cancel
    /// `Done` as equivalent terminal states. No broadcast event is emitted for
    /// the cancellation itself -- a tick-based renderer observes the new status
    /// on its next `snapshot()` read. Safe to call repeatedly (idempotent).
    pub fn cancel(&self, id: SessionId) {
        // Clone the Arc out so the DashMap shard lock is dropped before we
        // take the task and snapshot mutexes -- keeps lock ordering clean.
        let Some(state) = self.sessions.get(&id).map(|e| Arc::clone(e.value())) else {
            return;
        };
        // Peek the handle without removing it: leaving it in place keeps the
        // one-turn-per-session guard in send() effective until the aborted task
        // actually finishes, preventing a cancel-then-send race on the context.
        let guard = state.task.lock().expect("task lock");
        if let Some(handle) = guard.as_ref()
            && !handle.is_finished()
        {
            handle.abort();
            let mut snap = state.snapshot.lock().expect("snapshot lock");
            snap.status = SessionStatus::Cancelled;
        }
    }

    /// Cancel and remove a session. Worktree teardown (if any) is the caller's
    /// responsibility via `worktree::remove` BEFORE calling `close`; once the
    /// session entry is removed there is no longer a handle associating it with
    /// its worktree path, so calling `close` first leaves the worktree on disk.
    pub fn close(&self, id: SessionId) {
        self.cancel(id);
        self.sessions.remove(&id);
    }

    /// Reopen a persisted session into a live, idle session whose context is
    /// rehydrated from the store. Returns the new in-process id.
    ///
    /// The store's `i64` session id and the in-process `SessionId` (u64) are
    /// distinct: the new session continues persisting to the SAME store row
    /// (`stored_session_id`) while getting a fresh in-process id for the runtime.
    ///
    /// The snapshot's token and cost counters start at zero: they track spend
    /// for THIS resumed run, not the lifetime total of the store row. Historical
    /// cost is intentionally not rehydrated in v1.
    ///
    /// # Errors
    /// Returns `Err` if no session store is configured, or if `load_messages`
    /// fails (including when the store has no row for `stored_session_id`).
    pub fn resume(
        &self,
        stored_session_id: i64,
        label: impl Into<String>,
        cwd: PathBuf,
    ) -> anyhow::Result<SessionId> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no session store configured"))?;
        let messages = store.load_messages(stored_session_id)?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let label = label.into();

        let mut config = self.base_config.clone();
        config.cwd = cwd.clone();
        config.session_store = self.store.clone();
        config.session_id = Some(stored_session_id);

        let ctx = Arc::new(AsyncMutex::new(ConversationContext::from_history(
            config.system_prompt.clone(),
            self.tools.all_tool_schemas(),
            messages,
        )));

        let snapshot = SessionSnapshot {
            id,
            label: label.clone(),
            cwd: cwd.display().to_string(),
            status: SessionStatus::Idle,
            model: config.model.clone(),
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_usd: 0.0,
        };

        let state = Arc::new(SessionState {
            label,
            cwd,
            ctx,
            config,
            snapshot: std::sync::Mutex::new(snapshot),
            task: std::sync::Mutex::new(None),
        });
        self.sessions.insert(id, state);
        Ok(id)
    }

    /// Find a live in-process session currently persisting to the given store
    /// row (`stored_session_id`), if one exists. Used to avoid resuming the same
    /// stored session twice (which would interleave two threads into one row).
    pub fn find_by_stored_session_id(&self, stored_session_id: i64) -> Option<SessionId> {
        self.sessions
            .iter()
            .find(|e| e.value().config.session_id == Some(stored_session_id))
            .map(|e| *e.key())
    }

    /// Access the configured session store, if any. Renderers use this for the
    /// session browser (FTS5 search + resume).
    pub fn store(&self) -> Option<Arc<SessionStore>> {
        self.store.clone()
    }

    /// Number of messages in a session's live conversation context.
    ///
    /// Non-blocking: uses `try_lock`, so it returns `None` if the session is
    /// unknown OR if a turn currently holds the context lock. Callers (status
    /// displays, diagnostics) must treat `None` as "unavailable right now",
    /// not "zero messages".
    pub fn context_message_count(&self, id: SessionId) -> Option<usize> {
        let state = self.sessions.get(&id).map(|e| Arc::clone(e.value()))?;
        state.ctx.try_lock().ok().map(|c| c.message_count())
    }

    /// Number of live sessions.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True when there are no sessions.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Send a message to a session, running one full agent turn on a background
    /// task. Returns immediately -- the turn runs concurrently with OTHER
    /// sessions' turns. Events stream out on the broadcast channel and update
    /// the snapshot.
    ///
    /// One turn at a time per session: if a turn is already in flight for `id`,
    /// this is a no-op (it does NOT queue or interleave a second turn on the
    /// same context). Callers should gate input on session status. A no-op also
    /// if the session id is unknown.
    pub fn send(self: &Arc<Self>, id: SessionId, message: String) {
        let Some(state) = self.sessions.get(&id).map(|e| Arc::clone(e.value())) else {
            return;
        };

        // Guard: refuse to start a second concurrent turn on the same session.
        // A live (unfinished) task means a turn is in progress.
        {
            let task = state.task.lock().expect("task lock");
            if let Some(handle) = task.as_ref()
                && !handle.is_finished()
            {
                return;
            }
        }

        let provider = Arc::clone(&self.provider);
        let tools = Arc::clone(&self.tools);
        let pricing = Arc::clone(&self.pricing);
        let config = state.config.clone();
        let ctx = Arc::clone(&state.ctx);
        let events_tx = self.events_tx.clone();
        // Capture the session state Arc directly so the task updates the snapshot
        // without a per-event DashMap lookup.
        let task_state = Arc::clone(&state);

        let handle = tokio::spawn(async move {
            let stream =
                agent_turn_with_pricing(config, provider, tools, ctx, message, Some(pricing));
            futures::pin_mut!(stream);
            while let Some(event) = stream.next().await {
                // Update the live snapshot before fanning out so a renderer that
                // reads snapshots on tick sees state consistent with the event.
                // The std Mutex is taken and released within this statement --
                // never held across the .await above.
                task_state
                    .snapshot
                    .lock()
                    .expect("snapshot lock")
                    .apply(&event);
                // Best-effort fan-out: zero receivers returns Err, which we drop.
                let _ = events_tx.send(SessionEvent { id, event });
            }
        });

        *state.task.lock().expect("task lock") = Some(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_runtime::test_support::stub_manager;
    use tokio::sync::broadcast::error::RecvError;

    /// Drain events for `id` until a TurnEnd or Error arrives, collecting text.
    async fn drive_to_end(
        rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
        id: SessionId,
    ) -> String {
        let mut text = String::new();
        loop {
            match rx.recv().await {
                Ok(ev) if ev.id == id => match ev.event {
                    crate::types::AgentEvent::Text(t) => text.push_str(&t),
                    crate::types::AgentEvent::TurnEnd | crate::types::AgentEvent::Error(_) => break,
                    _ => {}
                },
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
        text
    }

    #[tokio::test]
    async fn send_streams_events_and_updates_snapshot() {
        let mgr = stub_manager();
        let mut rx = mgr.subscribe();
        let id = mgr.spawn("s", PathBuf::from("/tmp"));

        mgr.send(id, "hi".into());

        let text = drive_to_end(&mut rx, id).await;
        assert!(text.contains("hello from stub"), "got {text}");

        // After a turn ends the snapshot should be Done with counters populated.
        let snap = mgr.snapshot(id).expect("snapshot");
        assert_eq!(snap.status, SessionStatus::Done);
        assert!(snap.turns >= 1);
        assert!(snap.output_tokens >= 1);
    }

    #[tokio::test]
    async fn spawn_creates_idle_session() {
        let mgr = stub_manager();
        let id = mgr.spawn("alpha", PathBuf::from("/tmp"));
        let snap = mgr.snapshot(id).expect("snapshot exists");
        assert_eq!(snap.id, id);
        assert_eq!(snap.label, "alpha");
        assert_eq!(snap.status, SessionStatus::Idle);
        assert_eq!(mgr.len(), 1);
    }

    #[tokio::test]
    async fn spawn_assigns_distinct_ids() {
        let mgr = stub_manager();
        let a = mgr.spawn("a", PathBuf::from("/tmp"));
        let b = mgr.spawn("b", PathBuf::from("/tmp"));
        assert_ne!(a, b);
        assert_eq!(mgr.snapshots().len(), 2);
    }

    #[tokio::test]
    async fn three_sessions_run_independently() {
        let mgr = stub_manager();
        let mut rx = mgr.subscribe();
        let ids: Vec<SessionId> = (0..3)
            .map(|i| mgr.spawn(format!("s{i}"), PathBuf::from("/tmp")))
            .collect();

        for id in &ids {
            mgr.send(*id, "go".into());
        }

        // Each session runs on its own tokio task with its own ConversationContext,
        // so they execute concurrently; here we drain each to completion and assert
        // every one reached Done. (Parallelism is structural -- independent tasks --
        // not timing-asserted here.)
        for id in &ids {
            let _ = drive_to_end(&mut rx, *id).await;
        }
        for id in &ids {
            assert_eq!(mgr.snapshot(*id).unwrap().status, SessionStatus::Done);
        }
    }

    #[tokio::test]
    async fn cancel_marks_session_cancelled() {
        let mgr = stub_manager();
        let id = mgr.spawn("s", PathBuf::from("/tmp"));
        mgr.send(id, "go".into());
        // Cancel immediately; whether or not the turn already finished, status
        // must not be left mid-flight, and a second cancel must be safe.
        mgr.cancel(id);
        mgr.cancel(id);
        let status = mgr.snapshot(id).unwrap().status;
        assert!(
            matches!(status, SessionStatus::Cancelled | SessionStatus::Done),
            "got {status:?}"
        );
    }

    #[tokio::test]
    async fn find_by_stored_session_id_matches_live_session() {
        use synapse_provider::{ChatMessage, ContentBlock, Role};
        let dir = std::env::temp_dir().join(format!("syn-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            std::sync::Arc::new(synapse_session::SessionStore::open(&dir.join("s.db")).unwrap());
        let sid = store.create_session("proj", "stub-model").unwrap();
        let msg = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        };
        store.insert_turn(sid, &msg, 0, 0).unwrap();

        let provider: std::sync::Arc<dyn synapse_provider::Provider + Send + Sync> =
            std::sync::Arc::new(super::super::test_support::StubProvider { reply: "ok".into() });
        let mgr = SessionManager::new(
            provider,
            std::sync::Arc::new(synapse_tools::ToolRegistry::new()),
            std::sync::Arc::new(crate::cost::PricingTable::load()),
            Some(std::sync::Arc::clone(&store)),
            super::super::test_support::base_config(),
        );
        let id = mgr.resume(sid, "r", PathBuf::from("/tmp")).unwrap();
        assert_eq!(mgr.find_by_stored_session_id(sid), Some(id));
        assert_eq!(mgr.find_by_stored_session_id(999), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn resume_loads_history_into_context() {
        use synapse_provider::{ChatMessage, ContentBlock, Role};

        // Build a manager with a real on-disk store in a temp dir.
        let dir = std::env::temp_dir().join(format!("syn-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            std::sync::Arc::new(synapse_session::SessionStore::open(&dir.join("s.db")).unwrap());
        let sid = store.create_session("proj", "stub-model").unwrap();
        let msg = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "earlier message".into(),
            }],
        };
        store.insert_turn(sid, &msg, 0, 0).unwrap();

        let provider: std::sync::Arc<dyn synapse_provider::Provider + Send + Sync> =
            std::sync::Arc::new(super::super::test_support::StubProvider { reply: "ok".into() });
        let mgr = SessionManager::new(
            provider,
            std::sync::Arc::new(synapse_tools::ToolRegistry::new()),
            std::sync::Arc::new(crate::cost::PricingTable::load()),
            Some(std::sync::Arc::clone(&store)),
            super::super::test_support::base_config(),
        );

        let id = mgr.resume(sid, "resumed", PathBuf::from("/tmp")).unwrap();
        // The resumed context must already contain the historical message.
        let snap = mgr.snapshot(id).unwrap();
        assert_eq!(snap.status, SessionStatus::Idle);
        assert_eq!(mgr.context_message_count(id).unwrap(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }
}
