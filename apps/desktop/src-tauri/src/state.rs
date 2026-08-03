//! Native session and cache state that keeps Rift tokens outside the webview.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio_util::sync::CancellationToken;

use crate::gateway::RiftGateway;
use crate::model::{
    CommandError, CommandErrorKind, ConnectionProfile, DirectorySource, MessagePage,
    RoomConnectionStatus, RoomConversationCommandResult, RoomConversationEvent,
    RoomConversationEventEnvelope, RoomDirectorySnapshot, RoomMessage, RoomStatus,
};
use crate::rift::AuthenticatedRiftClient;

/// Fixed application-data filename for non-secret per-room read cursors.
const ROOM_READ_MARKERS_FILENAME: &str = "rift-room-read-markers.json";

/// Maximum number of recently updated room cursors retained on disk.
const MAX_ROOM_READ_MARKERS: usize = 500;

/// Maximum unique caller stream identifiers retained during one authenticated session.
const MAX_ROOM_STREAM_IDS_PER_SESSION: usize = 65_536;

/// Maximum deletion tombstones retained before the stream must be reopened.
const MAX_ROOM_DELETION_TOMBSTONES: usize = 4_096;

/// Maximum message-version ledger entries retained during one room generation.
const MAX_ROOM_MESSAGE_VERSION_ENTRIES: usize = 65_536;

/// One non-secret read cursor scoped to an origin, human, and room tuple.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoomReadMarker {
    /// Normalized Rift HTTP or HTTPS origin without credentials or a path.
    pub rift_origin: String,
    /// Signed-in Rift human identifier.
    pub user_id: String,
    /// Rift room identifier.
    pub room_id: String,
    /// Most recent loaded message explicitly marked read.
    pub last_read_message_id: String,
    /// UTC timestamp used for deterministic retention ordering.
    pub read_at: DateTime<Utc>,
}

/// Process-local secret state for the current Rift login.
pub struct AppState {
    /// Session, room generation, message window, and sole gateway replaced under one lock.
    inner: Mutex<NativeState>,
}

/// All mutable process-local ownership state guarded by one mutex.
struct NativeState {
    /// Authenticated Rift session when a login remains active.
    session: Option<ActiveRiftSession>,
    /// Monotonic generation allocated to room opens across session replacements.
    next_room_generation: u64,
}

/// One authenticated native session and its optional generation-scoped room.
struct ActiveRiftSession {
    /// Opaque shared native HTTP and token-rotation client.
    client: AuthenticatedRiftClient,
    /// Caller-generated room stream identifiers that cannot be reused within this session.
    used_room_stream_ids: HashSet<String>,
    /// Current room open attempt, message window, and sole gateway.
    room: Option<ActiveRoom>,
}

/// Mutable native state for exactly one room generation.
struct ActiveRoom {
    /// Opaque monotonic generation that distinguishes same-room reopen attempts.
    generation: u64,
    /// Caller-generated identifier known before the asynchronous open command completes.
    stream_id: String,
    /// Exact Rift room identifier bound to every operation and event.
    room_id: String,
    /// Shared cancellation signal for HTTP work, reconciliation, and the gateway.
    cancellation: CancellationToken,
    /// True after the initial oldest-first message page was installed once.
    page_installed: bool,
    /// True after the opening snapshot was captured and the gateway barrier released.
    snapshot_sealed: bool,
    /// Oldest-first bounded native message window.
    messages: Vec<RoomMessage>,
    /// Bounded deletion tombstones observed during this exact generation.
    deleted_message_ids: HashSet<String>,
    /// Latest full-or-partial message version observed for each bounded identifier.
    latest_message_versions: BTreeMap<String, chrono::DateTime<chrono::FixedOffset>>,
    /// Newest partial edit retained until a full message catches up.
    pending_message_updates: BTreeMap<String, PendingRoomMessageUpdate>,
    /// Whether the active window exposes an older-page cursor.
    has_older: bool,
    /// Latest native connection state included in snapshots and ordered events.
    connection_status: RoomConnectionStatus,
    /// Highest event-or-command sequence applied within this exact room generation.
    last_event_sequence: u64,
    /// Sole WebSocket writer for this room generation.
    gateway: Option<RiftGateway>,
}

/// One server edit retained so update-before-create ordering cannot regress later pages.
struct PendingRoomMessageUpdate {
    /// Replacement message body from the newest observed update.
    content: String,
    /// Parsed authoritative edit timestamp.
    edited_at: chrono::DateTime<chrono::FixedOffset>,
    /// Original RFC3339 edit timestamp preserved for the shared DTO.
    edited_at_text: String,
}

/// Current locked snapshot boundary paired with its native ordering cursor.
pub(crate) struct RoomSnapshotBoundary {
    /// Caller-generated one-use token shared by the snapshot and later updates.
    pub(crate) stream_id: String,
    /// Highest ordered event-or-command result already represented by this snapshot.
    pub(crate) last_event_sequence: u64,
    /// Current oldest-first native message window.
    pub(crate) page: MessagePage,
    /// Current native connection status after every pre-snapshot event.
    pub(crate) connection_status: RoomConnectionStatus,
}

/// Cloneable capability proving one exact session, room, and open generation.
#[derive(Clone)]
pub(crate) struct RoomLease {
    /// Authenticated client identity captured when the lease was issued.
    session: AuthenticatedRiftClient,
    /// Internal monotonic generation that never crosses the native boundary.
    generation: u64,
    /// Caller-generated identifier used for exact pre-response cancellation.
    stream_id: String,
    /// Exact Rift room identifier captured under the state lock.
    room_id: String,
    /// Cancellation signal shared by every operation in this generation.
    cancellation: CancellationToken,
}

/// Read-only accessors for native code holding a room capability.
impl RoomLease {
    /// Return the exact room identifier bound to this lease.
    pub(crate) fn room_id(&self) -> &str {
        &self.room_id
    }

    /// Return the opaque stream identifier required by every post-open command.
    pub(crate) fn stream_id(&self) -> &str {
        &self.stream_id
    }

    /// Borrow the shared cancellation signal for interruptible native work.
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    /// Borrow the authenticated client captured by the atomic room-open operation.
    pub(crate) fn session(&self) -> &AuthenticatedRiftClient {
        &self.session
    }
}

/// Cancel every operation and gateway owned by one room generation.
impl ActiveRoom {
    /// Signal room cancellation and synchronously stop the sole gateway owner.
    fn cancel(&mut self) {
        self.cancellation.cancel();
        if let Some(gateway) = self.gateway.take() {
            gateway.cancel();
        }
    }

    /// Create a cloneable capability for the room generation under the lock.
    fn lease(&self, session: &AuthenticatedRiftClient) -> RoomLease {
        RoomLease {
            session: session.clone(),
            generation: self.generation,
            stream_id: self.stream_id.clone(),
            room_id: self.room_id.clone(),
            cancellation: self.cancellation.clone(),
        }
    }

    /// Report whether a capability names this exact session and room generation.
    fn matches_lease(&self, session: &AuthenticatedRiftClient, lease: &RoomLease) -> bool {
        session.same_session(&lease.session)
            && self.generation == lease.generation
            && self.stream_id == lease.stream_id
            && self.room_id == lease.room_id
    }
}

/// Native session access and mutation operations.
impl AppState {
    /// Create an application state without an authenticated Rift session.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(NativeState {
                session: None,
                next_room_generation: 0,
            }),
        }
    }

    /// Clone the current native client handle without cloning independent tokens.
    pub fn session(&self) -> Result<Option<AuthenticatedRiftClient>, CommandError> {
        self.inner
            .lock()
            .map(|guard| guard.session.as_ref().map(|active| active.client.clone()))
            .map_err(|_| {
                CommandError::new(
                    CommandErrorKind::Storage,
                    "Henosis could not access the native session.",
                )
            })
    }

    /// Replace the current client and invalidate any previously cloned handle.
    pub fn set_session(&self, session: AuthenticatedRiftClient) -> Result<(), CommandError> {
        let mut guard = self.inner.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not update the native session.",
            )
        })?;
        if let Some(mut previous) = guard.session.replace(ActiveRiftSession {
            client: session,
            used_room_stream_ids: HashSet::new(),
            room: None,
        }) {
            if let Some(mut room) = previous.room.take() {
                room.cancel();
            }
            previous.client.invalidate();
        }
        Ok(())
    }

    /// Begin one room open only if the caller's authenticated session remains current.
    pub(crate) fn begin_room_open(
        &self,
        expected_session: &AuthenticatedRiftClient,
        room_id: &str,
        stream_id: &str,
    ) -> Result<RoomLease, CommandError> {
        if room_id.is_empty() {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "Choose a valid Rift room before opening the conversation.",
            ));
        }
        if !valid_room_stream_id(stream_id) {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "Start the room with a valid unique stream identifier.",
            ));
        }
        let mut guard = self.inner.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not update the native room.",
            )
        })?;
        let current_session = guard.session.as_ref().ok_or_else(|| {
            CommandError::new(
                CommandErrorKind::ConnectionRequired,
                "Connect to Rift before opening a room.",
            )
        })?;
        if !current_session.client.same_session(expected_session) {
            return Err(CommandError::new(
                CommandErrorKind::ConnectionRequired,
                "The Rift session changed. Refresh rooms and try again.",
            ));
        }
        if current_session.used_room_stream_ids.contains(stream_id) {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "That room stream identifier was already used. Start the room with a new one.",
            ));
        }
        if current_session.used_room_stream_ids.len() >= MAX_ROOM_STREAM_IDS_PER_SESSION {
            return Err(CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis reached the room stream limit for this session. Reconnect to continue.",
            ));
        }
        let generation = guard.next_room_generation.checked_add(1).ok_or_else(|| {
            CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis could not start another room operation. Reconnect to Rift and try again.",
            )
        })?;
        guard.next_room_generation = generation;
        let Some(active) = guard.session.as_mut() else {
            return Err(connection_required_error());
        };
        active.used_room_stream_ids.insert(stream_id.to_owned());
        if let Some(mut previous) = active.room.take() {
            previous.cancel();
        }
        let room = ActiveRoom {
            generation,
            stream_id: stream_id.to_owned(),
            room_id: room_id.to_owned(),
            cancellation: CancellationToken::new(),
            page_installed: false,
            snapshot_sealed: false,
            messages: Vec::new(),
            deleted_message_ids: HashSet::new(),
            latest_message_versions: BTreeMap::new(),
            pending_message_updates: BTreeMap::new(),
            has_older: false,
            connection_status: RoomConnectionStatus::Connecting,
            last_event_sequence: 0,
            gateway: None,
        };
        let lease = room.lease(&active.client);
        active.room = Some(room);
        Ok(lease)
    }

    /// Acquire a lease for one fully opened current room before starting transport work.
    pub(crate) fn room_operation(
        &self,
        room_id: &str,
        stream_id: &str,
    ) -> Result<RoomLease, CommandError> {
        let guard = self.inner.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not access the native room.",
            )
        })?;
        let active = guard
            .session
            .as_ref()
            .ok_or_else(connection_required_error)?;
        let room = active.room.as_ref().ok_or_else(stale_room_error)?;
        if room.room_id != room_id
            || room.stream_id != stream_id
            || room.cancellation.is_cancelled()
            || !room.page_installed
            || !room.snapshot_sealed
        {
            return Err(stale_room_error());
        }
        Ok(room.lease(&active.client))
    }

    /// Acquire a lease for the exact current room attempt before or after page installation.
    pub(crate) fn room_attempt(
        &self,
        room_id: &str,
        stream_id: &str,
    ) -> Result<RoomLease, CommandError> {
        let guard = self.inner.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not access the native room.",
            )
        })?;
        let active = guard
            .session
            .as_ref()
            .ok_or_else(connection_required_error)?;
        let room = active.room.as_ref().ok_or_else(stale_room_error)?;
        if room.room_id != room_id
            || room.stream_id != stream_id
            || room.cancellation.is_cancelled()
        {
            return Err(stale_room_error());
        }
        Ok(room.lease(&active.client))
    }

    /// Cancel one exact caller-known stream or reserve its unused token as pre-cancelled.
    pub(crate) fn cancel_room_stream(
        &self,
        room_id: &str,
        stream_id: &str,
    ) -> Result<(), CommandError> {
        if room_id.is_empty() || !valid_room_stream_id(stream_id) {
            return Err(stale_room_error());
        }
        let mut guard = self.lock_inner()?;
        let active = guard
            .session
            .as_mut()
            .ok_or_else(connection_required_error)?;
        let matches = active
            .room
            .as_ref()
            .is_some_and(|room| room.room_id == room_id && room.stream_id == stream_id);
        if matches {
            if let Some(mut room) = active.room.take() {
                room.cancel();
            }
            return Ok(());
        }
        if active.used_room_stream_ids.contains(stream_id) {
            return Err(stale_room_error());
        }
        if active.used_room_stream_ids.len() >= MAX_ROOM_STREAM_IDS_PER_SESSION {
            return Err(CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis reached the room stream limit for this session. Reconnect to continue.",
            ));
        }
        active.used_room_stream_ids.insert(stream_id.to_owned());
        Ok(())
    }

    /// Install exactly one initial oldest-first page for the current open attempt.
    pub(crate) fn install_room_page(
        &self,
        lease: &RoomLease,
        page: MessagePage,
    ) -> Result<(), CommandError> {
        let page = validate_room_page(lease.room_id(), page)?;
        let mut guard = self.lock_inner()?;
        let room = current_room_mut(&mut guard, lease)?;
        if room.page_installed {
            return Err(CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis already installed the initial room history.",
            ));
        }
        for message in &page.messages {
            remember_room_message_version(room, &message.id, room_message_version(message)?)?;
        }
        room.messages = page.messages;
        room.has_older = page.has_older;
        room.page_installed = true;
        Ok(())
    }

    /// Atomically install the sole gateway only while its room lease remains current.
    pub(crate) fn install_room_gateway(
        &self,
        lease: &RoomLease,
        gateway: RiftGateway,
    ) -> Result<(), CommandError> {
        let mut guard = self.lock_inner()?;
        let room = current_room_mut(&mut guard, lease)?;
        if !room.page_installed {
            return Err(stale_room_error());
        }
        if room.gateway.is_some() {
            return Err(CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis already installed the native room connection.",
            ));
        }
        room.gateway = Some(gateway);
        Ok(())
    }

    /// Cancel a failed open only when it still owns the exact active generation.
    pub(crate) fn abort_room_open(&self, lease: &RoomLease) -> Result<(), CommandError> {
        let mut guard = self.lock_inner()?;
        let Some(active) = guard.session.as_mut() else {
            return Ok(());
        };
        let should_abort = active
            .room
            .as_ref()
            .is_some_and(|room| room.matches_lease(&active.client, lease));
        if should_abort {
            if let Some(mut room) = active.room.take() {
                room.cancel();
            }
        }
        Ok(())
    }

    /// Merge an explicitly requested older page or replace the entire live window.
    pub(crate) fn merge_room_page(
        &self,
        lease: &RoomLease,
        page: MessagePage,
        replace_live_window: bool,
    ) -> Result<(), CommandError> {
        let page = validate_room_page(lease.room_id(), page)?;
        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        merge_page_into_room(room, page, replace_live_window, true)?;
        Ok(())
    }

    /// Merge and order older history while its request cursor remains the visible oldest message.
    pub(crate) fn merge_older_room_page(
        &self,
        lease: &RoomLease,
        before_message_id: &str,
        page: MessagePage,
    ) -> Result<RoomConversationCommandResult<MessagePage>, CommandError> {
        if before_message_id.is_empty() {
            return Err(invalid_room_message_error());
        }
        let page = validate_room_page(lease.room_id(), page)?;
        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        if room.messages.first().map(|message| message.id.as_str()) != Some(before_message_id) {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "The visible room history changed. Load older messages from its current start.",
            ));
        }
        commit_room_command_result(room, |room| merge_page_into_room(room, page, false, true))
    }

    /// Version-merge and order one complete command-result message.
    pub(crate) fn apply_room_message(
        &self,
        lease: &RoomLease,
        message: RoomMessage,
    ) -> Result<RoomConversationCommandResult<Option<RoomMessage>>, CommandError> {
        validate_room_message(lease.room_id(), &message)?;
        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        commit_room_command_result(room, |room| upsert_room_message(room, message))
    }

    /// Tombstone, remove, and order one command-result message identifier.
    pub(crate) fn delete_room_message(
        &self,
        lease: &RoomLease,
        message_id: &str,
    ) -> Result<RoomConversationCommandResult<String>, CommandError> {
        if message_id.is_empty() {
            return Err(invalid_room_message_error());
        }
        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        commit_room_command_result(room, |room| {
            tombstone_room_message(room, message_id)?;
            Ok(message_id.to_owned())
        })
    }

    /// Order one non-message command value after validating its exact active generation.
    pub(crate) fn sequence_room_command_result<T>(
        &self,
        lease: &RoomLease,
        value: T,
    ) -> Result<RoomConversationCommandResult<T>, CommandError> {
        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        commit_room_command_result(room, |_| Ok(value))
    }

    /// Apply one generation-scoped gateway event without an external delivery action.
    pub(crate) fn apply_room_event(
        &self,
        lease: &RoomLease,
        event: &RoomConversationEvent,
    ) -> Result<(), CommandError> {
        self.apply_room_event_before_release(lease, event, |_| true)
            .map(|_| ())
    }

    /// Apply and deliver one gateway event while replacement remains blocked by the state lock.
    pub(crate) fn apply_room_event_before_release<F>(
        &self,
        lease: &RoomLease,
        event: &RoomConversationEvent,
        deliver: F,
    ) -> Result<bool, CommandError>
    where
        F: FnOnce(&RoomConversationEventEnvelope) -> bool,
    {
        if room_event_id(event) != lease.room_id() {
            return Err(stale_room_error());
        }
        match event {
            RoomConversationEvent::MessageCreate { message, .. } => {
                validate_room_message(lease.room_id(), message)?;
            }
            RoomConversationEvent::MessageUpdate {
                message_id,
                edited_at,
                ..
            } => {
                if message_id.is_empty() || DateTime::parse_from_rfc3339(edited_at).is_err() {
                    return Err(invalid_room_message_error());
                }
            }
            RoomConversationEvent::MessageDelete { message_id, .. } => {
                if message_id.is_empty() {
                    return Err(invalid_room_message_error());
                }
            }
            RoomConversationEvent::Reconciliation { .. } => {}
            RoomConversationEvent::TypingStart { .. }
            | RoomConversationEvent::PresenceUpdate { .. }
            | RoomConversationEvent::UploadProgress { .. }
            | RoomConversationEvent::ConnectionChanged { .. } => {}
        }

        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        let effective_event = match event {
            RoomConversationEvent::MessageCreate { message, .. } => {
                upsert_room_message(room, message.clone())?.map(|message| {
                    RoomConversationEvent::MessageCreate {
                        room_id: lease.room_id().to_owned(),
                        message,
                    }
                })
            }
            RoomConversationEvent::MessageUpdate {
                message_id,
                content,
                edited_at,
                ..
            } => {
                if apply_room_message_update(room, message_id, content, edited_at)? {
                    Some(event.clone())
                } else {
                    None
                }
            }
            RoomConversationEvent::MessageDelete { message_id, .. } => {
                if tombstone_room_message(room, message_id)? {
                    Some(event.clone())
                } else {
                    None
                }
            }
            RoomConversationEvent::Reconciliation {
                page,
                replace_live_window,
                ..
            } => {
                let page = validate_room_page(lease.room_id(), page.clone())?;
                let page = merge_page_into_room(room, page, *replace_live_window, false)?;
                if *replace_live_window || !page.messages.is_empty() {
                    Some(RoomConversationEvent::Reconciliation {
                        room_id: lease.room_id().to_owned(),
                        page,
                        replace_live_window: *replace_live_window,
                    })
                } else {
                    None
                }
            }
            RoomConversationEvent::TypingStart { .. }
            | RoomConversationEvent::PresenceUpdate { .. }
            | RoomConversationEvent::UploadProgress { .. } => Some(event.clone()),
            RoomConversationEvent::ConnectionChanged { status, .. } => {
                room.connection_status = *status;
                Some(event.clone())
            }
        };
        let Some(effective_event) = effective_event else {
            return Ok(true);
        };
        let sequence = next_room_sequence(room)?;
        room.last_event_sequence = sequence;
        let envelope = RoomConversationEventEnvelope {
            stream_id: room.stream_id.clone(),
            sequence,
            event: effective_event,
        };
        let delivered = deliver(&envelope);
        drop(guard);
        Ok(delivered)
    }

    /// Commit a read marker while its exact generation and loaded ordering remain locked.
    pub(crate) fn commit_room_read_marker<F>(
        &self,
        lease: &RoomLease,
        message_id: &str,
        commit: F,
    ) -> Result<(), CommandError>
    where
        F: FnOnce(&[RoomMessage], usize) -> Result<(), CommandError>,
    {
        if message_id.is_empty() {
            return Err(invalid_room_message_error());
        }
        let guard = self.lock_inner()?;
        let room = current_installed_room(&guard, lease)?;
        let candidate_index = room
            .messages
            .iter()
            .position(|message| message.id == message_id)
            .ok_or_else(|| {
                CommandError::new(
                    CommandErrorKind::Validation,
                    "Mark a message that is currently loaded in the open room.",
                )
            })?;
        let result = commit(&room.messages, candidate_index);
        drop(guard);
        result
    }

    /// Clone the active oldest-first message window after generation validation.
    pub(crate) fn active_room_messages(
        &self,
        lease: &RoomLease,
    ) -> Result<Vec<RoomMessage>, CommandError> {
        let guard = self.lock_inner()?;
        Ok(current_installed_room(&guard, lease)?.messages.clone())
    }

    /// Clone the active oldest-first page and its older-history availability.
    pub(crate) fn active_room_page(&self, lease: &RoomLease) -> Result<MessagePage, CommandError> {
        let guard = self.lock_inner()?;
        let room = current_installed_room(&guard, lease)?;
        Ok(MessagePage {
            messages: room.messages.clone(),
            has_older: room.has_older,
        })
    }

    /// Seal one current snapshot and release its installed gateway before unlocking the room.
    pub(crate) fn seal_room_snapshot_boundary(
        &self,
        lease: &RoomLease,
    ) -> Result<RoomSnapshotBoundary, CommandError> {
        let mut guard = self.lock_inner()?;
        let room = current_installed_room_mut(&mut guard, lease)?;
        if room.snapshot_sealed {
            return Err(CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis already sealed the opening room snapshot.",
            ));
        }
        let boundary = RoomSnapshotBoundary {
            stream_id: room.stream_id.clone(),
            last_event_sequence: room.last_event_sequence,
            page: MessagePage {
                messages: room.messages.clone(),
                has_older: room.has_older,
            },
            connection_status: room.connection_status,
        };
        let gateway = room.gateway.as_mut().ok_or_else(stale_room_error)?;
        gateway.start().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Protocol,
                "Henosis could not start the native room connection.",
            )
        })?;
        room.snapshot_sealed = true;
        Ok(boundary)
    }

    /// Look up one message only within the active bounded room window.
    pub(crate) fn room_message(
        &self,
        lease: &RoomLease,
        message_id: &str,
    ) -> Result<Option<RoomMessage>, CommandError> {
        if message_id.is_empty() {
            return Err(invalid_room_message_error());
        }
        let guard = self.lock_inner()?;
        Ok(current_installed_room(&guard, lease)?
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .cloned())
    }

    /// Return the oldest loaded opaque message identifier for backward pagination.
    pub(crate) fn oldest_room_message_id(
        &self,
        lease: &RoomLease,
    ) -> Result<Option<String>, CommandError> {
        let guard = self.lock_inner()?;
        Ok(current_installed_room(&guard, lease)?
            .messages
            .first()
            .map(|message| message.id.clone()))
    }

    /// Return the newest loaded opaque message identifier for reconciliation and reads.
    pub(crate) fn newest_room_message_id(
        &self,
        lease: &RoomLease,
    ) -> Result<Option<String>, CommandError> {
        let guard = self.lock_inner()?;
        Ok(current_installed_room(&guard, lease)?
            .messages
            .last()
            .map(|message| message.id.clone()))
    }

    /// Enqueue one coalescing typing signal on the sole current gateway writer.
    pub(crate) fn send_room_typing(&self, lease: &RoomLease) -> Result<(), CommandError> {
        let guard = self.lock_inner()?;
        let room = current_installed_room(&guard, lease)?;
        let gateway = room.gateway.as_ref().ok_or_else(stale_room_error)?;
        gateway.enqueue_typing().map_err(|_| {
            CommandError::new(
                CommandErrorKind::ConnectionRequired,
                "The room connection is not ready for typing updates. Try again shortly.",
            )
        })
    }

    /// Close only the exact leased room generation and cancel all of its work.
    pub(crate) fn close_room(&self, lease: &RoomLease) -> Result<(), CommandError> {
        let mut guard = self.lock_inner()?;
        let active = guard
            .session
            .as_mut()
            .ok_or_else(connection_required_error)?;
        let matches = active
            .room
            .as_ref()
            .is_some_and(|room| room.matches_lease(&active.client, lease));
        if !matches {
            return Err(stale_room_error());
        }
        if let Some(mut room) = active.room.take() {
            room.cancel();
        }
        Ok(())
    }

    /// Cancel and remove the current room attempt and gateway without logging out.
    pub(crate) fn clear_gateway(&self) -> Result<(), CommandError> {
        let mut guard = self.lock_inner()?;
        if let Some(mut room) = guard.session.as_mut().and_then(|active| active.room.take()) {
            room.cancel();
        }
        Ok(())
    }

    /// Invalidate and remove all process-local Rift token state.
    pub fn clear_session(&self) -> Result<(), CommandError> {
        let mut guard = self.inner.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not clear the native session.",
            )
        })?;
        if let Some(mut session) = guard.session.take() {
            if let Some(mut room) = session.room.take() {
                room.cancel();
            }
            session.client.invalidate();
        }
        Ok(())
    }

    /// Lock native ownership state with one stable storage error.
    fn lock_inner(&self) -> Result<std::sync::MutexGuard<'_, NativeState>, CommandError> {
        self.inner.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not access the native room.",
            )
        })
    }
}

/// Construct the stable error used when no authenticated native client exists.
fn connection_required_error() -> CommandError {
    CommandError::new(
        CommandErrorKind::ConnectionRequired,
        "Connect to Rift before opening a room.",
    )
}

/// Construct the stable error used for every stale room or generation capability.
fn stale_room_error() -> CommandError {
    CommandError::new(
        CommandErrorKind::Validation,
        "The active Rift room changed. Open the room again.",
    )
}

/// Accept bounded URL-safe caller tokens suitable for one-use room stream identity.
fn valid_room_stream_id(stream_id: &str) -> bool {
    (16..=128).contains(&stream_id.len())
        && stream_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Construct the stable error used for an invalid sanitized message contract.
fn invalid_room_message_error() -> CommandError {
    CommandError::new(
        CommandErrorKind::Protocol,
        "Rift returned invalid room message data. Refresh the room and try again.",
    )
}

/// Borrow the exact current room for a lease without requiring initial installation.
fn current_room_mut<'a>(
    state: &'a mut NativeState,
    lease: &RoomLease,
) -> Result<&'a mut ActiveRoom, CommandError> {
    let active = state
        .session
        .as_mut()
        .ok_or_else(connection_required_error)?;
    let matches = active
        .room
        .as_ref()
        .is_some_and(|room| room.matches_lease(&active.client, lease));
    if !matches || lease.cancellation.is_cancelled() {
        return Err(stale_room_error());
    }
    active.room.as_mut().ok_or_else(stale_room_error)
}

/// Borrow the exact installed current room for one result or event mutation.
fn current_installed_room_mut<'a>(
    state: &'a mut NativeState,
    lease: &RoomLease,
) -> Result<&'a mut ActiveRoom, CommandError> {
    let room = current_room_mut(state, lease)?;
    if !room.page_installed {
        return Err(stale_room_error());
    }
    Ok(room)
}

/// Borrow the exact installed current room for one read-only operation.
fn current_installed_room<'a>(
    state: &'a NativeState,
    lease: &RoomLease,
) -> Result<&'a ActiveRoom, CommandError> {
    let active = state
        .session
        .as_ref()
        .ok_or_else(connection_required_error)?;
    let room = active.room.as_ref().ok_or_else(stale_room_error)?;
    if !room.matches_lease(&active.client, lease)
        || lease.cancellation.is_cancelled()
        || !room.page_installed
    {
        return Err(stale_room_error());
    }
    Ok(room)
}

/// Validate and canonicalize one page before any shared-state mutation occurs.
fn validate_room_page(room_id: &str, page: MessagePage) -> Result<MessagePage, CommandError> {
    let messages = canonicalize_room_messages(room_id, page.messages)?;
    Ok(MessagePage {
        messages,
        has_older: page.has_older,
    })
}

/// Validate one sanitized message's identity, room binding, and order key.
fn validate_room_message(room_id: &str, message: &RoomMessage) -> Result<(), CommandError> {
    if room_id.is_empty()
        || message.id.is_empty()
        || message.room_id != room_id
        || DateTime::parse_from_rfc3339(&message.created_at).is_err()
        || message
            .edited_at
            .as_deref()
            .is_some_and(|edited_at| DateTime::parse_from_rfc3339(edited_at).is_err())
    {
        return Err(invalid_room_message_error());
    }
    Ok(())
}

/// Deduplicate by message ID and return a deterministic oldest-first window.
fn canonicalize_room_messages(
    room_id: &str,
    messages: Vec<RoomMessage>,
) -> Result<Vec<RoomMessage>, CommandError> {
    let mut by_id = BTreeMap::new();
    for message in messages {
        validate_room_message(room_id, &message)?;
        if let Some(existing) = by_id.get_mut(&message.id) {
            if room_message_version(&message)? > room_message_version(existing)? {
                *existing = message;
            }
        } else {
            by_id.insert(message.id.clone(), message);
        }
    }
    let mut messages = by_id.into_values().collect::<Vec<_>>();
    messages.sort_by(compare_room_messages);
    Ok(messages)
}

/// Compare validated messages by server creation time and deterministic opaque ID tie-breaker.
fn compare_room_messages(left: &RoomMessage, right: &RoomMessage) -> Ordering {
    match (
        DateTime::parse_from_rfc3339(&left.created_at),
        DateTime::parse_from_rfc3339(&right.created_at),
    ) {
        (Ok(left_created), Ok(right_created)) => left_created
            .cmp(&right_created)
            .then_with(|| left.id.cmp(&right.id)),
        _ => left
            .created_at
            .cmp(&right.created_at)
            .then_with(|| left.id.cmp(&right.id)),
    }
}

/// Merge or replace one validated page with an explicit older-cursor policy.
fn merge_page_into_room(
    room: &mut ActiveRoom,
    page: MessagePage,
    replace_live_window: bool,
    update_has_older_on_merge: bool,
) -> Result<MessagePage, CommandError> {
    let MessagePage {
        messages,
        has_older,
    } = page;
    if replace_live_window {
        let mut effective_messages = Vec::with_capacity(messages.len());
        for message in messages {
            let Some(message) = normalize_incoming_room_message(room, message)? else {
                continue;
            };
            let effective = if let Some(existing) = room
                .messages
                .iter()
                .find(|existing| existing.id == message.id)
            {
                newer_room_message(existing, message)?
            } else {
                message
            };
            effective_messages.push(effective);
        }
        effective_messages = canonicalize_room_messages(&room.room_id, effective_messages)?;
        room.messages.clone_from(&effective_messages);
        room.has_older = has_older;
        return Ok(MessagePage {
            messages: effective_messages,
            has_older,
        });
    }
    let mut effective_messages = Vec::with_capacity(messages.len());
    for message in messages {
        if let Some(effective) = upsert_room_message(room, message)? {
            effective_messages.push(effective);
        }
    }
    effective_messages = canonicalize_room_messages(&room.room_id, effective_messages)?;
    if update_has_older_on_merge {
        room.has_older = has_older;
    }
    Ok(MessagePage {
        messages: effective_messages,
        has_older,
    })
}

/// Insert a non-tombstoned message or retain the newest loaded version with that identifier.
fn upsert_room_message(
    room: &mut ActiveRoom,
    message: RoomMessage,
) -> Result<Option<RoomMessage>, CommandError> {
    let Some(message) = normalize_incoming_room_message(room, message)? else {
        return Ok(None);
    };
    if let Some(existing) = room
        .messages
        .iter_mut()
        .find(|existing| existing.id == message.id)
    {
        *existing = newer_room_message(existing, message)?;
        let effective = existing.clone();
        room.messages.sort_by(compare_room_messages);
        return Ok(Some(effective));
    } else {
        room.messages.push(message.clone());
    }
    room.messages.sort_by(compare_room_messages);
    Ok(Some(message))
}

/// Select the strictly newer full message while retaining the loaded value on a version tie.
fn newer_room_message(
    existing: &RoomMessage,
    incoming: RoomMessage,
) -> Result<RoomMessage, CommandError> {
    if room_message_version(&incoming)? > room_message_version(existing)? {
        Ok(incoming)
    } else {
        Ok(existing.clone())
    }
}

/// Parse the authoritative edit-or-create timestamp from one validated room message.
fn room_message_version(
    message: &RoomMessage,
) -> Result<chrono::DateTime<chrono::FixedOffset>, CommandError> {
    DateTime::parse_from_rfc3339(message.edited_at.as_deref().unwrap_or(&message.created_at))
        .map_err(|_| invalid_room_message_error())
}

/// Apply one partial edit only when its server timestamp advances the loaded message version.
fn apply_room_message_update(
    room: &mut ActiveRoom,
    message_id: &str,
    content: &str,
    edited_at: &str,
) -> Result<bool, CommandError> {
    if room.deleted_message_ids.contains(message_id) {
        return Ok(false);
    }
    let edited_version =
        DateTime::parse_from_rfc3339(edited_at).map_err(|_| invalid_room_message_error())?;
    if room
        .latest_message_versions
        .get(message_id)
        .is_some_and(|known| edited_version <= *known)
    {
        return Ok(false);
    }
    remember_room_message_version(room, message_id, edited_version)?;
    room.pending_message_updates.insert(
        message_id.to_owned(),
        PendingRoomMessageUpdate {
            content: content.to_owned(),
            edited_at: edited_version,
            edited_at_text: edited_at.to_owned(),
        },
    );
    if let Some(message) = room
        .messages
        .iter_mut()
        .find(|message| message.id == message_id)
    {
        message.content = content.to_owned();
        message.edited_at = Some(edited_at.to_owned());
    }
    Ok(true)
}

/// Apply retained edit knowledge and reject a full message older than the native version ledger.
fn normalize_incoming_room_message(
    room: &mut ActiveRoom,
    mut message: RoomMessage,
) -> Result<Option<RoomMessage>, CommandError> {
    if room.deleted_message_ids.contains(&message.id) {
        return Ok(None);
    }
    let incoming_version = room_message_version(&message)?;
    if let Some(update) = room.pending_message_updates.get(&message.id)
        && update.edited_at >= incoming_version
    {
        message.content.clone_from(&update.content);
        message.edited_at = Some(update.edited_at_text.clone());
    }
    let effective_version = room_message_version(&message)?;
    if let Some(known_version) = room.latest_message_versions.get(&message.id)
        && *known_version > effective_version
    {
        return Ok(room
            .messages
            .iter()
            .find(|existing| existing.id == message.id)
            .cloned());
    }
    let message_id = message.id.clone();
    remember_room_message_version(room, &message_id, effective_version)?;
    Ok(Some(message))
}

/// Retain one monotonic message version under a fixed per-generation memory bound.
fn remember_room_message_version(
    room: &mut ActiveRoom,
    message_id: &str,
    version: chrono::DateTime<chrono::FixedOffset>,
) -> Result<(), CommandError> {
    if !room.latest_message_versions.contains_key(message_id)
        && room.latest_message_versions.len() >= MAX_ROOM_MESSAGE_VERSION_ENTRIES
    {
        return Err(CommandError::new(
            CommandErrorKind::Protocol,
            "Henosis reached the room message-version limit. Reopen the room and try again.",
        ));
    }
    room.latest_message_versions
        .entry(message_id.to_owned())
        .and_modify(|known| {
            if version > *known {
                *known = version;
            }
        })
        .or_insert(version);
    Ok(())
}

/// Record one new deletion before removing any matching loaded message.
fn tombstone_room_message(room: &mut ActiveRoom, message_id: &str) -> Result<bool, CommandError> {
    if room.deleted_message_ids.contains(message_id) {
        return Ok(false);
    }
    if room.deleted_message_ids.len() >= MAX_ROOM_DELETION_TOMBSTONES {
        return Err(CommandError::new(
            CommandErrorKind::Protocol,
            "Henosis reached the room deletion limit. Reopen the room and try again.",
        ));
    }
    room.deleted_message_ids.insert(message_id.to_owned());
    room.latest_message_versions.remove(message_id);
    room.pending_message_updates.remove(message_id);
    room.messages.retain(|message| message.id != message_id);
    Ok(true)
}

/// Return the next shared event-or-command sequence without mutating native state.
fn next_room_sequence(room: &ActiveRoom) -> Result<u64, CommandError> {
    room.last_event_sequence.checked_add(1).ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::Protocol,
            "Henosis could not order another room update. Reopen the room and try again.",
        )
    })
}

/// Commit one command mutation and expose its value at the same shared room sequence.
fn commit_room_command_result<T, F>(
    room: &mut ActiveRoom,
    commit: F,
) -> Result<RoomConversationCommandResult<T>, CommandError>
where
    F: FnOnce(&mut ActiveRoom) -> Result<T, CommandError>,
{
    let sequence = next_room_sequence(room)?;
    let value = commit(room)?;
    room.last_event_sequence = sequence;
    Ok(RoomConversationCommandResult {
        stream_id: room.stream_id.clone(),
        sequence,
        value,
    })
}

/// Return the exact room identifier carried by one sanitized conversation event.
fn room_event_id(event: &RoomConversationEvent) -> &str {
    match event {
        RoomConversationEvent::MessageCreate { room_id, .. }
        | RoomConversationEvent::MessageUpdate { room_id, .. }
        | RoomConversationEvent::MessageDelete { room_id, .. }
        | RoomConversationEvent::TypingStart { room_id, .. }
        | RoomConversationEvent::PresenceUpdate { room_id, .. }
        | RoomConversationEvent::UploadProgress { room_id, .. }
        | RoomConversationEvent::ConnectionChanged { room_id, .. }
        | RoomConversationEvent::Reconciliation { room_id, .. } => room_id,
    }
}

/// Resolve one application-data file without accepting caller-controlled paths.
fn app_data_file(app: &AppHandle, filename: &str) -> Result<PathBuf, CommandError> {
    let directory = app.path().app_data_dir().map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not locate its application data directory.",
        )
    })?;
    fs::create_dir_all(&directory).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not prepare its application data directory.",
        )
    })?;
    Ok(directory.join(filename))
}

/// Read one optional native JSON record.
fn read_json<T: DeserializeOwned>(
    app: &AppHandle,
    filename: &str,
) -> Result<Option<T>, CommandError> {
    let path = app_data_file(app, filename)?;
    let content = match fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not read its saved connection data.",
            ));
        }
    };
    serde_json::from_slice(&content).map(Some).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis found invalid saved connection data.",
        )
    })
}

/// Write one native JSON record without placing it in browser storage.
fn write_json<T: Serialize>(
    app: &AppHandle,
    filename: &str,
    value: &T,
) -> Result<(), CommandError> {
    let path = app_data_file(app, filename)?;
    let content = serde_json::to_vec_pretty(value).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not encode its connection data.",
        )
    })?;
    fs::write(path, content).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not save its connection data.",
        )
    })
}

/// Normalize and validate the Rift origin stored in a read-marker key.
fn normalize_marker_origin(origin: &str) -> Result<String, CommandError> {
    crate::rift::normalize_endpoint(origin)
        .map(|url| url.origin().ascii_serialization())
        .map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis found an invalid Rift origin in room history state. Reconnect to Rift and try again.",
            )
        })
}

/// Deduplicate, normalize, deterministically order, and bound read markers.
fn canonicalize_room_read_markers(
    markers: &[RoomReadMarker],
) -> Result<Vec<RoomReadMarker>, CommandError> {
    let mut keyed = BTreeMap::<(String, String, String), RoomReadMarker>::new();
    for source in markers {
        if source.user_id.is_empty()
            || source.room_id.is_empty()
            || source.last_read_message_id.is_empty()
        {
            return Err(CommandError::new(
                CommandErrorKind::Storage,
                "Henosis found an incomplete room history marker. Reopen the room and mark it read again.",
            ));
        }
        let mut marker = source.clone();
        marker.rift_origin = normalize_marker_origin(&marker.rift_origin)?;
        let key = (
            marker.rift_origin.clone(),
            marker.user_id.clone(),
            marker.room_id.clone(),
        );
        match keyed.get_mut(&key) {
            Some(current)
                if marker.read_at > current.read_at
                    || (marker.read_at == current.read_at
                        && marker.last_read_message_id > current.last_read_message_id) =>
            {
                *current = marker;
            }
            Some(_) => {}
            None => {
                keyed.insert(key, marker);
            }
        }
    }

    let mut canonical = keyed.into_values().collect::<Vec<_>>();
    canonical.sort_by(|left, right| {
        right
            .read_at
            .cmp(&left.read_at)
            .then_with(|| left.rift_origin.cmp(&right.rift_origin))
            .then_with(|| left.user_id.cmp(&right.user_id))
            .then_with(|| left.room_id.cmp(&right.room_id))
    });
    canonical.truncate(MAX_ROOM_READ_MARKERS);
    Ok(canonical)
}

/// Read and validate markers from one fixed native path.
fn read_room_read_markers_at(path: &Path) -> Result<Vec<RoomReadMarker>, CommandError> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not read room history state. Check application-data permissions and try again.",
            ));
        }
    };
    let markers = serde_json::from_slice::<Vec<RoomReadMarker>>(&content).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            format!(
                "Henosis found invalid room history state. Move {ROOM_READ_MARKERS_FILENAME} aside and reopen Henosis."
            ),
        )
    })?;
    canonicalize_room_read_markers(&markers)
}

/// Atomically replace one marker file through a synced temporary sibling.
fn write_room_read_markers_at(path: &Path, markers: &[RoomReadMarker]) -> Result<(), CommandError> {
    let parent = path.parent().ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not locate room history storage. Reopen the application and try again.",
        )
    })?;
    fs::create_dir_all(parent).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not prepare room history storage. Check application-data permissions and try again.",
        )
    })?;
    let canonical = canonicalize_room_read_markers(markers)?;
    let content = serde_json::to_vec_pretty(&canonical).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not encode room history state. Reopen the room and try again.",
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".rift-room-read-markers-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not create temporary room history storage. Check free space and application-data permissions, then try again.",
            )
        })?;
    temporary.write_all(&content).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not write room history state. Check free space and try again.",
        )
    })?;
    temporary.as_file().sync_all().map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not flush room history state. Check the application-data filesystem and try again.",
        )
    })?;
    temporary.persist(path).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not replace room history state. Check application-data permissions and try again.",
        )
    })?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis saved room history but could not flush its directory. Check the application-data filesystem before retrying.",
            )
        })?;
    Ok(())
}

/// Load the saved non-secret Rift endpoint and username.
pub fn read_profile(app: &AppHandle) -> Result<Option<ConnectionProfile>, CommandError> {
    read_json(app, "rift-profile.json")
}

/// Save only non-secret Rift profile fields.
pub fn write_profile(app: &AppHandle, profile: &ConnectionProfile) -> Result<(), CommandError> {
    write_json(app, "rift-profile.json", profile)
}

/// Load cached rooms and force their provenance and connectivity flags offline.
pub fn read_room_cache(app: &AppHandle) -> Result<Option<RoomDirectorySnapshot>, CommandError> {
    read_json::<RoomDirectorySnapshot>(app, "rift-room-cache.json").map(|cached| {
        cached.map(|mut snapshot| {
            snapshot.source = DirectorySource::Cached;
            snapshot.connected = false;
            snapshot.rooms.iter_mut().for_each(|room| {
                if !matches!(room.status, RoomStatus::Paused) {
                    room.status = RoomStatus::Disconnected;
                }
            });
            snapshot
        })
    })
}

/// Save a sanitized room snapshot that contains no Rift tokens.
pub fn write_room_cache(
    app: &AppHandle,
    snapshot: &RoomDirectorySnapshot,
) -> Result<(), CommandError> {
    write_json(app, "rift-room-cache.json", snapshot)
}

/// Load bounded non-secret room read markers from native application data.
pub fn read_room_read_markers(app: &AppHandle) -> Result<Vec<RoomReadMarker>, CommandError> {
    read_room_read_markers_at(&app_data_file(app, ROOM_READ_MARKERS_FILENAME)?)
}

/// Canonicalize and atomically save bounded non-secret room read markers.
pub fn write_room_read_markers(
    app: &AppHandle,
    markers: &[RoomReadMarker],
) -> Result<(), CommandError> {
    write_room_read_markers_at(&app_data_file(app, ROOM_READ_MARKERS_FILENAME)?, markers)
}

#[cfg(test)]
/// Exercises bounded, identity-scoped native room read-marker persistence.
mod tests {
    use std::collections::BTreeSet;

    use chrono::{Duration, TimeZone, Utc};
    use url::Url;

    use super::*;

    /// Construct one marker at a stable time offset for ordering assertions.
    fn marker(
        rift_origin: &str,
        user_id: &str,
        room_id: &str,
        message_id: &str,
        seconds: i64,
    ) -> RoomReadMarker {
        RoomReadMarker {
            rift_origin: rift_origin.into(),
            user_id: user_id.into(),
            room_id: room_id.into(),
            last_read_message_id: message_id.into(),
            read_at: Utc
                .timestamp_opt(1_700_000_000, 0)
                .single()
                .expect("base marker time must exist")
                + Duration::seconds(seconds),
        }
    }

    /// Resolve a marker file inside an isolated temporary sibling directory.
    fn marker_test_path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("temporary marker directory must exist");
        let path = directory.path().join(ROOM_READ_MARKERS_FILENAME);
        (directory, path)
    }

    /// Construct one isolated authenticated client for native room ownership tests.
    fn room_session(user_id: &str) -> AuthenticatedRiftClient {
        AuthenticatedRiftClient::gateway_test_client(
            Url::parse("https://rift.example/").expect("room fixture origin must parse"),
            user_id,
        )
    }

    /// Construct one sanitized room message with a deterministic order key.
    fn room_message(id: &str, room_id: &str, created_at: &str) -> RoomMessage {
        RoomMessage {
            id: id.into(),
            room_id: room_id.into(),
            author_id: "user-2".into(),
            author_username: "collaborator".into(),
            author_display_name: Some("Collaborator".into()),
            author_avatar_url: None,
            content: format!("content for {id}"),
            edited_at: None,
            created_at: created_at.into(),
            message_type: "user".into(),
            attachments: Vec::new(),
        }
    }

    /// Return message identifiers from an oldest-first native page.
    fn message_ids(page: &MessagePage) -> Vec<&str> {
        page.messages
            .iter()
            .map(|message| message.id.as_str())
            .collect()
    }

    /// Construct one installed room window for atomic transition tests.
    fn installed_room_state() -> (
        AppState,
        AuthenticatedRiftClient,
        RoomLease,
        RoomSnapshotBoundary,
    ) {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("atomic test session must install");
        let lease = state
            .begin_room_open(&session, "room-1", "stream-state-0001")
            .expect("atomic test room must begin");
        state
            .install_room_page(
                &lease,
                MessagePage {
                    messages: vec![
                        room_message("message-1", "room-1", "2026-08-03T12:01:00Z"),
                        room_message("message-2", "room-1", "2026-08-03T12:02:00Z"),
                    ],
                    has_older: true,
                },
            )
            .expect("atomic test page must install");
        state
            .install_room_gateway(&lease, crate::gateway::test_closed_rift_gateway())
            .expect("atomic test gateway must install");
        let boundary = state
            .seal_room_snapshot_boundary(&lease)
            .expect("atomic test snapshot must seal");
        (state, session, lease, boundary)
    }

    /// An older response cannot merge after reconciliation changes its requested cursor.
    #[test]
    fn older_page_merge_revalidates_the_visible_cursor() {
        let (state, _session, lease, _boundary) = installed_room_state();
        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::Reconciliation {
                    room_id: "room-1".into(),
                    page: MessagePage {
                        messages: vec![room_message("message-9", "room-1", "2026-08-03T12:09:00Z")],
                        has_older: true,
                    },
                    replace_live_window: true,
                },
            )
            .expect("bounded fallback must replace the native window");

        let stale = state.merge_older_room_page(
            &lease,
            "message-1",
            MessagePage {
                messages: vec![room_message("message-0", "room-1", "2026-08-03T12:00:00Z")],
                has_older: false,
            },
        );

        assert!(matches!(
            stale,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert_eq!(
            message_ids(
                &state
                    .active_room_page(&lease)
                    .expect("replacement window must remain readable"),
            ),
            vec!["message-9"]
        );
    }

    /// A sealed snapshot watermark is followed by contiguous envelopes on the same stream.
    #[test]
    fn sealed_snapshot_orders_every_following_event() {
        let (state, _session, lease, boundary) = installed_room_state();
        let mut envelopes = Vec::new();

        for user_id in ["user-2", "user-3"] {
            state
                .apply_room_event_before_release(
                    &lease,
                    &RoomConversationEvent::TypingStart {
                        room_id: "room-1".into(),
                        user_id: user_id.into(),
                        username: "collaborator".into(),
                    },
                    |envelope| {
                        envelopes.push(envelope.clone());
                        true
                    },
                )
                .expect("post-snapshot event must apply");
        }

        assert_eq!(boundary.stream_id, lease.stream_id());
        assert_eq!(boundary.last_event_sequence, 0);
        assert_eq!(envelopes.len(), 2);
        assert!(
            envelopes
                .iter()
                .all(|envelope| envelope.stream_id == boundary.stream_id)
        );
        assert_eq!(
            envelopes
                .iter()
                .map(|envelope| envelope.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// Event delivery holds room ownership until the event has crossed the native boundary.
    #[test]
    fn event_delivery_blocks_same_room_replacement_until_release() {
        let (state, session, lease, _boundary) = installed_room_state();
        std::thread::scope(|scope| {
            let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
            let (completed_tx, completed_rx) = std::sync::mpsc::channel();
            let worker_attempted_tx = attempted_tx.clone();
            let worker_completed_tx = completed_tx.clone();
            let worker_state = &state;
            let worker_session = &session;
            let delivered = state
                .apply_room_event_before_release(
                    &lease,
                    &RoomConversationEvent::TypingStart {
                        room_id: "room-1".into(),
                        user_id: "user-2".into(),
                        username: "collaborator".into(),
                    },
                    |_| {
                        scope.spawn(move || {
                            worker_attempted_tx
                                .send(())
                                .expect("replacement attempt must be observable");
                            let replacement = worker_state.begin_room_open(
                                worker_session,
                                "room-1",
                                "stream-state-0002",
                            );
                            worker_completed_tx
                                .send(replacement)
                                .expect("replacement result must be observable");
                        });
                        attempted_rx
                            .recv()
                            .expect("replacement thread must reach the state boundary");
                        assert!(completed_rx.try_recv().is_err());
                        true
                    },
                )
                .expect("current gateway event must apply");
            assert!(delivered);
            let replacement = completed_rx
                .recv()
                .expect("replacement must finish after event delivery")
                .expect("same-room replacement must succeed");
            assert!(lease.cancellation().is_cancelled());
            state
                .abort_room_open(&replacement)
                .expect("replacement cleanup must succeed");
        });
    }

    /// Read-marker persistence holds the validated message window through its commit callback.
    #[test]
    fn read_marker_commit_blocks_same_room_replacement_until_release() {
        let (state, session, lease, _boundary) = installed_room_state();
        std::thread::scope(|scope| {
            let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
            let (completed_tx, completed_rx) = std::sync::mpsc::channel();
            let worker_attempted_tx = attempted_tx.clone();
            let worker_completed_tx = completed_tx.clone();
            let worker_state = &state;
            let worker_session = &session;
            state
                .commit_room_read_marker(&lease, "message-2", |messages, candidate_index| {
                    assert_eq!(messages[candidate_index].id, "message-2");
                    scope.spawn(move || {
                        worker_attempted_tx
                            .send(())
                            .expect("replacement attempt must be observable");
                        let replacement = worker_state.begin_room_open(
                            worker_session,
                            "room-1",
                            "stream-state-0002",
                        );
                        worker_completed_tx
                            .send(replacement)
                            .expect("replacement result must be observable");
                    });
                    attempted_rx
                        .recv()
                        .expect("replacement thread must reach the state boundary");
                    assert!(completed_rx.try_recv().is_err());
                    Ok(())
                })
                .expect("loaded marker commit must succeed");
            let replacement = completed_rx
                .recv()
                .expect("replacement must finish after marker commit")
                .expect("same-room replacement must succeed");
            assert!(lease.cancellation().is_cancelled());
            state
                .abort_room_open(&replacement)
                .expect("replacement cleanup must succeed");
        });
    }

    /// Same-room replacement cancels the old actor and blocks every stale post-result mutation.
    #[tokio::test]
    async fn same_room_reopen_is_generation_scoped() {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("room session must install");
        let first = state
            .begin_room_open(&session, "room-1", "stream-reopen-0001")
            .expect("first room generation must begin");
        state
            .install_room_page(
                &first,
                MessagePage {
                    messages: vec![room_message(
                        "message-old",
                        "room-1",
                        "2026-08-03T12:00:00Z",
                    )],
                    has_older: false,
                },
            )
            .expect("first page must install");
        let (first_gateway, gateway_cancelled) = crate::gateway::test_rift_gateway();
        state
            .install_room_gateway(&first, first_gateway)
            .expect("first gateway must install");

        let replacement = state
            .begin_room_open(&session, "room-1", "stream-reopen-0002")
            .expect("same room must allocate a new generation");
        assert!(first.cancellation().is_cancelled());
        assert!(gateway_cancelled.is_cancelled());
        state
            .abort_room_open(&first)
            .expect("stale abort must be a harmless no-op");
        state
            .install_room_page(
                &replacement,
                MessagePage {
                    messages: vec![room_message(
                        "message-current",
                        "room-1",
                        "2026-08-03T12:01:00Z",
                    )],
                    has_older: false,
                },
            )
            .expect("replacement page must remain installable");

        assert!(
            state
                .apply_room_message(
                    &first,
                    room_message("message-stale", "room-1", "2026-08-03T12:02:00Z"),
                )
                .is_err()
        );
        assert!(
            state
                .apply_room_event(
                    &first,
                    &RoomConversationEvent::MessageDelete {
                        room_id: "room-1".into(),
                        message_id: "message-current".into(),
                    },
                )
                .is_err()
        );
        assert_eq!(
            message_ids(
                &state
                    .active_room_page(&replacement)
                    .expect("replacement page must remain current"),
            ),
            vec!["message-current"]
        );

        state
            .close_room(&replacement)
            .expect("current room generation must close");
        assert!(replacement.cancellation().is_cancelled());
        assert!(
            state
                .room_operation("room-1", replacement.stream_id())
                .is_err()
        );
    }

    /// A stale open compare-and-swap cannot replace or cancel a newer authenticated session.
    #[test]
    fn stale_session_cannot_begin_room_open() {
        let state = AppState::new();
        let first_session = room_session("user-1");
        let current_session = room_session("user-2");
        state
            .set_session(first_session.clone())
            .expect("first session must install");
        state
            .set_session(current_session.clone())
            .expect("current session must replace the first");
        let current = state
            .begin_room_open(&current_session, "room-current", "stream-current-0001")
            .expect("current session room must begin");
        state
            .install_room_page(
                &current,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("current room page must install");
        state
            .install_room_gateway(&current, crate::gateway::test_closed_rift_gateway())
            .expect("current room gateway must install");
        state
            .seal_room_snapshot_boundary(&current)
            .expect("current room snapshot must seal");

        let stale_error = state
            .begin_room_open(&first_session, "room-stale", "stream-stale-00001")
            .err()
            .expect("stale session must fail its room-open compare-and-swap");
        assert!(matches!(
            stale_error.kind,
            CommandErrorKind::ConnectionRequired
        ));
        assert!(!current.cancellation().is_cancelled());
        assert_eq!(
            state
                .room_operation("room-current", current.stream_id())
                .expect("current room must survive stale open")
                .room_id(),
            "room-current"
        );
    }

    /// A close-before-open reservation rejects that future token without disturbing the current room.
    #[test]
    fn pre_cancelled_stream_cannot_begin_or_cancel_the_current_room() {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("stream-guard session must install");
        let current = state
            .begin_room_open(&session, "room-current", "stream-current-guard-0001")
            .expect("current stream must begin");

        state
            .cancel_room_stream("room-future", "stream-future-guard-0001")
            .expect("close-before-open must reserve its fresh token");
        let rejected = state.begin_room_open(&session, "room-future", "stream-future-guard-0001");
        let reused = state.begin_room_open(&session, "room-current", "stream-current-guard-0001");
        let invalid = state.cancel_room_stream("room-current", "short");

        assert!(matches!(
            rejected,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(matches!(
            reused,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(invalid.is_err());
        assert!(!current.cancellation().is_cancelled());
        assert!(
            state
                .room_attempt("room-current", current.stream_id())
                .is_ok()
        );
    }

    /// Exact close removes a retained room even when an earlier failure already cancelled it.
    #[test]
    fn exact_close_removes_an_already_cancelled_current_stream() {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("cancelled-room session must install");
        let current = state
            .begin_room_open(&session, "room-current", "stream-cancelled-guard-0001")
            .expect("cancelled-room stream must begin");
        current.cancellation().cancel();

        state
            .cancel_room_stream(current.room_id(), current.stream_id())
            .expect("exact close must remove its already-cancelled room owner");

        assert!(
            state
                .room_attempt(current.room_id(), current.stream_id())
                .is_err()
        );
    }

    /// Closing can acquire and cancel the current room while its initial page is still pending.
    #[test]
    fn uninstalled_room_attempt_can_be_closed() {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("pending-open session must install");
        let pending = state
            .begin_room_open(&session, "room-1", "stream-pending-0001")
            .expect("pending room must begin");
        assert!(state.room_operation("room-1", pending.stream_id()).is_err());

        let close_lease = state
            .room_attempt("room-1", pending.stream_id())
            .expect("current pending attempt must be leasable for close");
        state
            .close_room(&close_lease)
            .expect("pending open must close");
        assert!(pending.cancellation().is_cancelled());
        assert!(close_lease.cancellation().is_cancelled());
        assert!(state.room_attempt("room-1", pending.stream_id()).is_err());
    }

    /// A closed outbound mailbox maps to one stable path-free command error.
    #[test]
    fn typing_enqueue_failure_is_stable_and_opaque() {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("typing session must install");
        let lease = state
            .begin_room_open(&session, "room-1", "stream-typing-00001")
            .expect("typing room must begin");
        state
            .install_room_page(
                &lease,
                MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                },
            )
            .expect("typing page must install");
        state
            .install_room_gateway(&lease, crate::gateway::test_closed_rift_gateway())
            .expect("closed test gateway must install");

        let error = state
            .send_room_typing(&lease)
            .expect_err("closed typing mailbox must return a command error");
        assert!(matches!(error.kind, CommandErrorKind::ConnectionRequired));
        assert_eq!(
            error.message,
            "The room connection is not ready for typing updates. Try again shortly."
        );
        let serialized = serde_json::to_string(&error).expect("typing error must serialize");
        assert!(!serialized.contains("channel"));
        assert!(!serialized.contains("socket"));
        assert!(!serialized.contains("closed"));
    }

    /// Unknown deletes tombstone future pages and creates while preserving a visible envelope.
    #[test]
    fn delete_before_page_or_create_cannot_resurrect_a_message() {
        let (state, _session, lease, _boundary) = installed_room_state();
        let mut envelopes = Vec::new();

        state
            .apply_room_event_before_release(
                &lease,
                &RoomConversationEvent::MessageDelete {
                    room_id: "room-1".into(),
                    message_id: "message-late".into(),
                },
                |envelope| {
                    envelopes.push(envelope.clone());
                    true
                },
            )
            .expect("unknown delete must be retained and delivered");
        state
            .merge_room_page(
                &lease,
                MessagePage {
                    messages: vec![room_message(
                        "message-late",
                        "room-1",
                        "2026-08-03T12:03:00Z",
                    )],
                    has_older: false,
                },
                false,
            )
            .expect("late page must merge without its tombstoned message");
        let delivered_create = state
            .apply_room_event_before_release(
                &lease,
                &RoomConversationEvent::MessageCreate {
                    room_id: "room-1".into(),
                    message: room_message("message-late", "room-1", "2026-08-03T12:04:00Z"),
                },
                |_| panic!("tombstoned create must not cross the native boundary"),
            )
            .expect("tombstoned create must be a harmless no-op");

        assert!(delivered_create);
        assert_eq!(envelopes.len(), 1);
        assert!(matches!(
            envelopes[0].event,
            RoomConversationEvent::MessageDelete { ref message_id, .. }
                if message_id == "message-late"
        ));
        assert!(
            state
                .room_message(&lease, "message-late")
                .expect("room lookup must succeed")
                .is_none()
        );
    }

    /// An update-before-create envelope patches a later stale fallback page before delivery.
    #[test]
    fn update_before_create_is_retained_across_reconciliation() {
        let (state, _session, lease, _boundary) = installed_room_state();
        let mut envelopes = Vec::new();

        state
            .apply_room_event_before_release(
                &lease,
                &RoomConversationEvent::MessageUpdate {
                    room_id: "room-1".into(),
                    message_id: "message-late".into(),
                    content: "newest content".into(),
                    edited_at: "2026-08-03T12:05:00Z".into(),
                },
                |envelope| {
                    envelopes.push(envelope.clone());
                    true
                },
            )
            .expect("unknown update must be retained and delivered");
        state
            .apply_room_event_before_release(
                &lease,
                &RoomConversationEvent::Reconciliation {
                    room_id: "room-1".into(),
                    page: MessagePage {
                        messages: vec![{
                            let mut message =
                                room_message("message-late", "room-1", "2026-08-03T12:03:00Z");
                            message.edited_at = Some("2026-08-03T12:05:00Z".into());
                            message
                        }],
                        has_older: false,
                    },
                    replace_live_window: true,
                },
                |envelope| {
                    envelopes.push(envelope.clone());
                    true
                },
            )
            .expect("fallback page must preserve the pending edit");

        let retained = state
            .room_message(&lease, "message-late")
            .expect("room lookup must succeed")
            .expect("reconciled message must be loaded");
        assert_eq!(retained.content, "newest content");
        assert_eq!(retained.edited_at.as_deref(), Some("2026-08-03T12:05:00Z"));
        assert_eq!(
            envelopes
                .iter()
                .map(|envelope| envelope.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(matches!(
            &envelopes[1].event,
            RoomConversationEvent::Reconciliation { page, .. }
                if page.messages[0].content == "newest content"
        ));
    }

    /// Late command results share event ordering and cannot regress or resurrect messages.
    #[test]
    fn command_results_preserve_newer_edits_and_deletions() {
        let (state, _session, lease, _boundary) = installed_room_state();
        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::MessageUpdate {
                    room_id: "room-1".into(),
                    message_id: "message-2".into(),
                    content: "gateway edit".into(),
                    edited_at: "2026-08-03T12:05:00Z".into(),
                },
            )
            .expect("newer gateway edit must apply");
        let mut stale = room_message("message-2", "room-1", "2026-08-03T12:02:00Z");
        stale.content = "stale response".into();
        stale.edited_at = Some("2026-08-03T12:03:00Z".into());
        let merged = state
            .apply_room_message(&lease, stale.clone())
            .expect("late command result must merge");
        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::MessageDelete {
                    room_id: "room-1".into(),
                    message_id: "message-2".into(),
                },
            )
            .expect("gateway delete must apply");
        let deleted = state
            .apply_room_message(&lease, stale)
            .expect("late deleted response must become an ordered no-op");

        assert_eq!(merged.sequence, 2);
        assert_eq!(
            merged
                .value
                .expect("newer loaded message must remain visible")
                .content,
            "gateway edit"
        );
        assert_eq!(deleted.sequence, 4);
        assert!(deleted.value.is_none());
        assert!(
            state
                .room_message(&lease, "message-2")
                .expect("room lookup must succeed")
                .is_none()
        );
    }

    /// Page merges and live events maintain one deduplicated oldest-first native window.
    #[test]
    fn native_room_window_merges_orders_and_reconciles() {
        let state = AppState::new();
        let session = room_session("user-1");
        state
            .set_session(session.clone())
            .expect("room session must install");
        let lease = state
            .begin_room_open(&session, "room-1", "stream-window-00001")
            .expect("room generation must begin");
        state
            .install_room_page(
                &lease,
                MessagePage {
                    messages: vec![
                        room_message("message-2", "room-1", "2026-08-03T12:02:00Z"),
                        room_message("message-1", "room-1", "2026-08-03T12:01:00Z"),
                    ],
                    has_older: true,
                },
            )
            .expect("unordered initial page must canonicalize");
        assert_eq!(
            state
                .oldest_room_message_id(&lease)
                .expect("oldest cursor must be readable")
                .as_deref(),
            Some("message-1")
        );
        assert_eq!(
            state
                .newest_room_message_id(&lease)
                .expect("newest cursor must be readable")
                .as_deref(),
            Some("message-2")
        );

        let mut replacement_message = room_message("message-1", "room-1", "2026-08-03T12:01:00Z");
        replacement_message.content = "canonical duplicate".into();
        replacement_message.edited_at = Some("2026-08-03T12:01:30Z".into());
        state
            .merge_room_page(
                &lease,
                MessagePage {
                    messages: vec![
                        replacement_message,
                        room_message("message-0", "room-1", "2026-08-03T12:00:00Z"),
                    ],
                    has_older: false,
                },
                false,
            )
            .expect("older page must merge");
        let merged = state
            .active_room_page(&lease)
            .expect("merged window must be readable");
        assert_eq!(
            message_ids(&merged),
            vec!["message-0", "message-1", "message-2"]
        );
        assert!(!merged.has_older);
        assert_eq!(merged.messages[1].content, "canonical duplicate");

        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::MessageUpdate {
                    room_id: "room-1".into(),
                    message_id: "message-2".into(),
                    content: "edited content".into(),
                    edited_at: "2026-08-03T12:03:00Z".into(),
                },
            )
            .expect("current edit event must apply");
        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::MessageDelete {
                    room_id: "room-1".into(),
                    message_id: "message-1".into(),
                },
            )
            .expect("current delete event must apply");
        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::Reconciliation {
                    room_id: "room-1".into(),
                    page: MessagePage {
                        messages: vec![room_message("message-3", "room-1", "2026-08-03T12:03:00Z")],
                        has_older: true,
                    },
                    replace_live_window: false,
                },
            )
            .expect("bounded reconciliation page must merge");
        let merged_events = state
            .active_room_page(&lease)
            .expect("event-updated window must be readable");
        assert_eq!(
            message_ids(&merged_events),
            vec!["message-0", "message-2", "message-3"]
        );
        assert!(!merged_events.has_older);
        assert_eq!(merged_events.messages[1].content, "edited content");

        let before_wrong_room = merged_events.clone();
        assert!(
            state
                .apply_room_event(
                    &lease,
                    &RoomConversationEvent::MessageDelete {
                        room_id: "room-2".into(),
                        message_id: "message-2".into(),
                    },
                )
                .is_err()
        );
        assert_eq!(
            state
                .active_room_page(&lease)
                .expect("wrong-room event must not mutate state"),
            before_wrong_room
        );

        state
            .apply_room_event(
                &lease,
                &RoomConversationEvent::Reconciliation {
                    room_id: "room-1".into(),
                    page: MessagePage {
                        messages: vec![room_message("message-4", "room-1", "2026-08-03T12:04:00Z")],
                        has_older: true,
                    },
                    replace_live_window: true,
                },
            )
            .expect("fallback reconciliation must replace the live window");
        let replaced = state
            .active_room_page(&lease)
            .expect("replaced window must be readable");
        assert_eq!(message_ids(&replaced), vec!["message-4"]);
        assert!(replaced.has_older);
    }

    /// Compound scope keeps accounts and origins separate while newest duplicates win.
    #[test]
    fn room_read_markers_key_by_origin_user_and_room() {
        let markers = canonicalize_room_read_markers(&[
            marker(
                "https://rift.example/",
                "user-1",
                "room-1",
                "message-old",
                1,
            ),
            marker("https://rift.example", "user-1", "room-1", "message-new", 2),
            marker(
                "https://rift.example",
                "user-2",
                "room-1",
                "message-other-user",
                3,
            ),
            marker(
                "https://other.example",
                "user-1",
                "room-1",
                "message-other-origin",
                4,
            ),
        ])
        .expect("valid markers must canonicalize");

        assert_eq!(markers.len(), 3);
        assert_eq!(markers[0].last_read_message_id, "message-other-origin");
        assert_eq!(markers[1].last_read_message_id, "message-other-user");
        assert_eq!(markers[2].last_read_message_id, "message-new");
        assert_eq!(markers[2].rift_origin, "https://rift.example");
    }

    /// Only the five hundred most recently updated room keys survive.
    #[test]
    fn room_read_markers_keep_five_hundred_most_recent_rooms() {
        let markers = (0..502)
            .map(|index| {
                marker(
                    "https://rift.example",
                    "user-1",
                    &format!("room-{index}"),
                    &format!("message-{index}"),
                    index,
                )
            })
            .collect::<Vec<_>>();

        let bounded =
            canonicalize_room_read_markers(&markers).expect("generated markers must canonicalize");
        assert_eq!(bounded.len(), MAX_ROOM_READ_MARKERS);
        assert_eq!(bounded.first().expect("newest marker").room_id, "room-501");
        assert_eq!(
            bounded.last().expect("oldest retained marker").room_id,
            "room-2"
        );
        assert!(!bounded.iter().any(|marker| marker.room_id == "room-1"));
    }

    /// Repeated writes replace the file and persist only the approved marker fields.
    #[test]
    fn room_read_markers_replace_through_a_temporary_sibling() {
        let (_directory, path) = marker_test_path();
        write_room_read_markers_at(
            &path,
            &[marker(
                "https://rift.example",
                "user-1",
                "room-1",
                "message-old",
                1,
            )],
        )
        .expect("initial marker file must be written");
        write_room_read_markers_at(
            &path,
            &[marker(
                "https://rift.example",
                "user-1",
                "room-1",
                "message-new",
                2,
            )],
        )
        .expect("marker file must be replaced");

        let stored = read_room_read_markers_at(&path).expect("markers must be readable");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].last_read_message_id, "message-new");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("marker JSON must remain on disk"))
                .expect("marker JSON must parse");
        let keys = value[0]
            .as_object()
            .expect("marker must be an object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            BTreeSet::from([
                "lastReadMessageId",
                "readAt",
                "riftOrigin",
                "roomId",
                "userId",
            ])
        );
        assert_eq!(
            fs::read_dir(path.parent().expect("marker parent must exist"))
                .expect("marker parent must be readable")
                .count(),
            1
        );
    }

    /// Invalid saved JSON produces actionable storage guidance.
    #[test]
    fn room_read_markers_report_invalid_saved_state() {
        let (_directory, path) = marker_test_path();
        fs::write(&path, b"{not-json").expect("invalid fixture must be written");

        let error =
            read_room_read_markers_at(&path).expect_err("invalid marker JSON must be rejected");
        assert!(matches!(error.kind, CommandErrorKind::Storage));
        assert!(error.message.contains(ROOM_READ_MARKERS_FILENAME));
        assert!(error.message.contains("Move"));
    }
}
