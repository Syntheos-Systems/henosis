//! Tauri commands that expose sanitized Henosis operations to the webview.

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use futures_util::future::BoxFuture;
use tauri::{AppHandle, Emitter, State};

use crate::gateway::{ROOM_CONVERSATION_EVENT, RiftGateway, RiftGatewayError, spawn_rift_gateway};
use crate::model::{
    BootstrapResult, CommandError, CommandErrorKind, MessagePage, PendingRoomAttachment,
    RiftConnectionInput, RoomConversationCommandResult, RoomConversationEvent,
    RoomConversationEventEnvelope, RoomConversationSnapshot, RoomDirectorySnapshot, RoomMessage,
    RoomPermissions, RoomSummary,
};
use crate::reconcile::{
    MessagePageSource, RECONCILE_PAGE_SIZE, ReconcileError, reconcile_open_room, unread_boundary,
};
use crate::rift::{self, RiftError};
use crate::state::{
    AppState, RoomLease, RoomReadMarker, read_profile, read_room_cache, read_room_read_markers,
    write_profile, write_room_cache, write_room_read_markers,
};

/// Serializes read-marker comparisons and atomic file replacements inside this process.
static ROOM_READ_MARKER_LOCK: Mutex<()> = Mutex::new(());

/// Generates path-free transfer identifiers for native upload progress events.
static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

/// Native transport operations delegated by the narrow room commands.
trait RoomCommandTransport: MessagePageSource {
    /// Fetch server-authoritative capabilities for the signed-in room member.
    fn permissions<'a>(
        &'a self,
        server_id: &'a str,
    ) -> BoxFuture<'a, Result<RoomPermissions, RiftError>>;

    /// Fetch one oldest-first page before an opaque loaded cursor.
    fn older<'a>(
        &'a self,
        room_id: &'a str,
        before_message_id: &'a str,
    ) -> BoxFuture<'a, Result<MessagePage, RiftError>>;

    /// Create one text, attachment-only, or combined room message.
    fn create<'a>(
        &'a self,
        room_id: &'a str,
        content: &'a str,
        pending_upload_ids: &'a [String],
    ) -> BoxFuture<'a, Result<RoomMessage, RiftError>>;

    /// Replace one room message body.
    fn edit<'a>(
        &'a self,
        room_id: &'a str,
        message_id: &'a str,
        content: &'a str,
    ) -> BoxFuture<'a, Result<RoomMessage, RiftError>>;

    /// Delete one room message.
    fn delete<'a>(
        &'a self,
        room_id: &'a str,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<(), RiftError>>;

    /// Upload selected native paths and return only server-staged metadata.
    fn upload<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<Vec<PendingRoomAttachment>, RiftError>>;
}

/// Delegate production command work to the authenticated native Rift client.
impl RoomCommandTransport for rift::AuthenticatedRiftClient {
    /// Fetch effective permissions from Rift's member-scoped endpoint.
    fn permissions<'a>(
        &'a self,
        server_id: &'a str,
    ) -> BoxFuture<'a, Result<RoomPermissions, RiftError>> {
        Box::pin(rift::room_permissions(self, server_id))
    }

    /// Fetch at most one explicit older-history page.
    fn older<'a>(
        &'a self,
        room_id: &'a str,
        before_message_id: &'a str,
    ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
        Box::pin(rift::messages_before(
            self,
            room_id,
            before_message_id,
            RECONCILE_PAGE_SIZE,
        ))
    }

    /// Create a message through Rift's replayable native HTTP boundary.
    fn create<'a>(
        &'a self,
        room_id: &'a str,
        content: &'a str,
        pending_upload_ids: &'a [String],
    ) -> BoxFuture<'a, Result<RoomMessage, RiftError>> {
        Box::pin(rift::create_message(
            self,
            room_id,
            content,
            pending_upload_ids,
        ))
    }

    /// Edit a message through Rift's replayable native HTTP boundary.
    fn edit<'a>(
        &'a self,
        room_id: &'a str,
        message_id: &'a str,
        content: &'a str,
    ) -> BoxFuture<'a, Result<RoomMessage, RiftError>> {
        Box::pin(rift::edit_message(self, room_id, message_id, content))
    }

    /// Delete a message through Rift's replayable native HTTP boundary.
    fn delete<'a>(
        &'a self,
        room_id: &'a str,
        message_id: &'a str,
    ) -> BoxFuture<'a, Result<(), RiftError>> {
        Box::pin(rift::delete_message(self, room_id, message_id))
    }

    /// Upload paths only inside the native process.
    fn upload<'a>(
        &'a self,
        paths: &'a [PathBuf],
    ) -> BoxFuture<'a, Result<Vec<PendingRoomAttachment>, RiftError>> {
        Box::pin(rift::upload_attachments(self, paths))
    }
}

/// Native file-selection seam that keeps dialog paths out of the command contract.
trait RoomAttachmentPicker {
    /// Return selected native paths or None when the person cancels the dialog.
    fn pick<'a>(&'a self) -> BoxFuture<'a, Option<Vec<PathBuf>>>;
}

/// Production asynchronous desktop file picker.
struct NativeRoomAttachmentPicker;

/// Open Rift attachment selection through the operating system portal.
impl RoomAttachmentPicker for NativeRoomAttachmentPicker {
    /// Convert native file handles into process-local paths only.
    fn pick<'a>(&'a self) -> BoxFuture<'a, Option<Vec<PathBuf>>> {
        Box::pin(async move {
            rfd::AsyncFileDialog::new()
                .set_title("Select up to 10 room attachments")
                .pick_files()
                .await
                .map(|files| {
                    files
                        .into_iter()
                        .map(|file| file.path().to_owned())
                        .collect()
                })
        })
    }
}

/// Sanitized room-event seam used by production Tauri emission and deterministic tests.
trait RoomCommandEventSink {
    /// Publish one ordered sanitized room envelope and report whether delivery succeeded.
    fn emit_room_event(&self, envelope: &RoomConversationEventEnvelope) -> bool;
}

/// Native operations needed to cross from bounded history into one sealed room stream.
trait RoomOpenRuntime {
    /// Read the current human-scoped marker without exposing its storage path.
    fn read_cursor(&self, lease: &RoomLease) -> Result<Option<String>, CommandError>;

    /// Construct the generation-owned gateway while retaining native application ownership.
    fn spawn_gateway(
        &self,
        lease: RoomLease,
        server_ids: Vec<String>,
        last_message: Option<&RoomMessage>,
    ) -> Result<RiftGateway, RiftGatewayError>;
}

/// Use the application handle for production marker storage and Tauri event delivery.
impl RoomOpenRuntime for AppHandle {
    /// Read one non-secret cursor from the application data directory.
    fn read_cursor(&self, lease: &RoomLease) -> Result<Option<String>, CommandError> {
        read_cursor_for_room(self, lease)
    }

    /// Spawn the sole deferred gateway actor owned by the room generation.
    fn spawn_gateway(
        &self,
        lease: RoomLease,
        server_ids: Vec<String>,
        last_message: Option<&RoomMessage>,
    ) -> Result<RiftGateway, RiftGatewayError> {
        spawn_rift_gateway(self.clone(), lease, server_ids, last_message)
    }
}

/// Persistence seam for generation-locked room read-marker updates.
trait RoomReadMarkerStore {
    /// Read every retained marker through one path-safe native boundary.
    fn read_markers(&self) -> Result<Vec<RoomReadMarker>, CommandError>;

    /// Atomically replace retained markers through one path-safe native boundary.
    fn write_markers(&self, markers: &[RoomReadMarker]) -> Result<(), CommandError>;
}

/// Persist read markers beneath Tauri's native application data directory.
impl RoomReadMarkerStore for AppHandle {
    /// Read and validate every retained native marker.
    fn read_markers(&self) -> Result<Vec<RoomReadMarker>, CommandError> {
        read_room_read_markers(self)
    }

    /// Canonicalize and atomically persist the retained marker set.
    fn write_markers(&self, markers: &[RoomReadMarker]) -> Result<(), CommandError> {
        write_room_read_markers(self, markers)
    }
}

/// Emit command-owned updates on the same fixed room event channel as the gateway.
impl RoomCommandEventSink for AppHandle {
    /// Emit through the fixed channel and report serialization or runtime failure.
    fn emit_room_event(&self, envelope: &RoomConversationEventEnvelope) -> bool {
        if Emitter::emit(self, ROOM_CONVERSATION_EVENT, envelope.clone()).is_err() {
            tracing::debug!(
                phase = "command-emit",
                "Room command event had no active recipient"
            );
            return false;
        }
        true
    }
}

/// Build one stable stale-room error without exposing native state internals.
fn stale_room_error() -> CommandError {
    CommandError::new(
        CommandErrorKind::Validation,
        "That room is no longer open. Open it again before continuing.",
    )
}

/// Reject empty or whitespace-padded opaque identifiers before native side effects.
fn validate_opaque_id(value: &str, message: &'static str) -> Result<(), CommandError> {
    if value.is_empty() || value.trim() != value {
        return Err(CommandError::new(CommandErrorKind::Validation, message));
    }
    Ok(())
}

/// Validate text and pending upload identifiers before constructing a transport future.
fn validate_send_input(content: &str, pending_upload_ids: &[String]) -> Result<(), CommandError> {
    if content.trim().is_empty() && pending_upload_ids.is_empty() {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "A room message needs text or at least one attachment.",
        ));
    }
    if pending_upload_ids.len() > rift::MAX_NATIVE_UPLOAD_FILES {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "A room message can include no more than 10 attachments.",
        ));
    }
    let mut unique_ids = HashSet::with_capacity(pending_upload_ids.len());
    for upload_id in pending_upload_ids {
        validate_opaque_id(upload_id, "A pending attachment identifier was invalid.")?;
        if !unique_ids.insert(upload_id.as_str()) {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "A pending attachment can be used only once per message.",
            ));
        }
    }
    Ok(())
}

/// Validate selected paths and return basename-plus-size metadata for safe progress events.
fn validate_selected_uploads(paths: &[PathBuf]) -> Result<Vec<(String, u64)>, CommandError> {
    if paths.is_empty() {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Select at least one attachment to upload.",
        ));
    }
    if paths.len() > rift::MAX_NATIVE_UPLOAD_FILES {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Select no more than 10 attachments at once.",
        ));
    }

    let mut total_bytes = 0_u64;
    let mut metadata = Vec::with_capacity(paths.len());
    for path in paths {
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| {
                !name.is_empty()
                    && !name
                        .chars()
                        .any(|character| matches!(character, '/' | '\\' | '\0' | '\r' | '\n'))
            })
            .ok_or_else(|| {
                CommandError::new(
                    CommandErrorKind::Validation,
                    "Select files with valid display names.",
                )
            })?
            .to_owned();
        let file_metadata = std::fs::symlink_metadata(path).map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not inspect one selected attachment.",
            )
        })?;
        if file_metadata.file_type().is_symlink() || !file_metadata.is_file() {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "Selected attachments must be regular files.",
            ));
        }
        let size_bytes = file_metadata.len();
        if size_bytes == 0 {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "Selected attachments cannot be empty.",
            ));
        }
        total_bytes = total_bytes.checked_add(size_bytes).ok_or_else(|| {
            CommandError::new(
                CommandErrorKind::Validation,
                "Selected attachments cannot exceed 100 MiB in one upload.",
            )
        })?;
        if total_bytes > rift::MAX_NATIVE_UPLOAD_BYTES {
            return Err(CommandError::new(
                CommandErrorKind::Validation,
                "Selected attachments cannot exceed 100 MiB in one upload.",
            ));
        }
        metadata.push((filename, size_bytes));
    }
    Ok(metadata)
}

/// Build a new opaque progress identifier without incorporating any native path.
fn next_transfer_id() -> String {
    let sequence = NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed);
    format!("native-upload-{sequence:016x}")
}

/// Apply, order, and emit one command-owned event or invalidate its incomplete stream.
fn emit_command_room_event<E: RoomCommandEventSink>(
    state: &AppState,
    lease: &RoomLease,
    sink: &E,
    event: RoomConversationEvent,
) -> Result<(), CommandError> {
    let delivered = state.apply_room_event_before_release(lease, &event, |envelope| {
        sink.emit_room_event(envelope)
    })?;
    if delivered {
        return Ok(());
    }
    lease.cancellation().cancel();
    Err(CommandError::new(
        CommandErrorKind::Protocol,
        "Henosis could not deliver the ordered room update. Reopen the room and try again.",
    ))
}

/// Race one Rift request against the generation-scoped room cancellation signal.
async fn await_room_request<T, F>(lease: &RoomLease, request: F) -> Result<T, CommandError>
where
    F: Future<Output = Result<T, RiftError>>,
{
    tokio::select! {
        biased;
        _ = lease.cancellation().cancelled() => Err(stale_room_error()),
        result = request => result.map_err(Into::into),
    }
}

/// Convert bounded reconciliation failures into stable command errors.
fn reconcile_command_error(error: ReconcileError) -> CommandError {
    match error {
        ReconcileError::Cancelled => stale_room_error(),
        ReconcileError::Rift(error) => error.into(),
    }
}

/// Convert gateway setup failures without revealing transport internals.
fn gateway_command_error(_error: RiftGatewayError) -> CommandError {
    CommandError::new(
        CommandErrorKind::Protocol,
        "Henosis could not start the native room connection.",
    )
}

/// Resolve one cached room only when it belongs to the exact active Rift identity.
fn cached_room_for_session(
    app: &AppHandle,
    session: &rift::AuthenticatedRiftClient,
    room_id: &str,
) -> Result<RoomSummary, CommandError> {
    validate_opaque_id(room_id, "Choose a valid room before opening it.")?;
    let directory = read_room_cache(app)?.ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::Validation,
            "Refresh the room directory before opening a room.",
        )
    })?;
    let connection = directory.connection.as_ref().ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::Validation,
            "Refresh the room directory before opening a room.",
        )
    })?;
    let profile = rift::profile_for(session);
    if connection.user_id != session.gateway_user_id() || connection.endpoint != profile.endpoint {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Refresh the room directory for the current Rift account before opening a room.",
        ));
    }
    let room = directory
        .rooms
        .into_iter()
        .find(|room| room.id == room_id)
        .ok_or_else(|| {
            CommandError::new(
                CommandErrorKind::Validation,
                "That room is no longer in the current Rift directory. Refresh rooms and try again.",
            )
        })?;
    validate_opaque_id(
        &room.server_id,
        "That room has an invalid server assignment. Refresh rooms and try again.",
    )?;
    Ok(room)
}

/// Resolve an authorized cached room before replacing the current native room generation.
fn begin_resolved_room_open<F>(
    state: &AppState,
    session: &rift::AuthenticatedRiftClient,
    room_id: &str,
    stream_id: &str,
    resolve_room: F,
) -> Result<(RoomLease, RoomSummary), CommandError>
where
    F: FnOnce() -> Result<RoomSummary, CommandError>,
{
    let room = resolve_room()?;
    let lease = state.begin_room_open(session, room_id, stream_id)?;
    Ok((lease, room))
}

/// Derive the canonical native read-marker scope from one authenticated lease.
fn marker_scope(lease: &RoomLease) -> Result<(String, String), CommandError> {
    let endpoint = rift::profile_for(lease.session()).endpoint;
    let origin = rift::normalize_endpoint(&endpoint)
        .map(|url| url.origin().ascii_serialization())
        .map_err(CommandError::from)?;
    Ok((origin, lease.session().gateway_user_id()))
}

/// Find the current generation's bounded read cursor without fetching older history.
fn read_cursor_for_room(
    app: &AppHandle,
    lease: &RoomLease,
) -> Result<Option<String>, CommandError> {
    let (rift_origin, user_id) = marker_scope(lease)?;
    Ok(read_room_read_markers(app)?
        .into_iter()
        .find(|marker| {
            marker.rift_origin == rift_origin
                && marker.user_id == user_id
                && marker.room_id == lease.room_id()
        })
        .map(|marker| marker.last_read_message_id))
}

/// Build a setup result from non-secret profile and cached-room state.
fn setup_bootstrap(app: &AppHandle) -> Result<BootstrapResult, CommandError> {
    Ok(BootstrapResult {
        saved_profile: read_profile(app)?,
        directory: read_room_cache(app)?,
        requires_authentication: true,
    })
}

/// Load live native state when possible and otherwise expose honest cached setup data.
#[tauri::command]
pub async fn bootstrap(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<BootstrapResult, CommandError> {
    let Some(session) = state.session()? else {
        return setup_bootstrap(&app);
    };

    match rift::fetch_room_directory(&session).await {
        Ok(directory) => {
            write_room_cache(&app, &directory)?;
            Ok(BootstrapResult {
                saved_profile: Some(rift::profile_for(&session)),
                directory: Some(directory),
                requires_authentication: false,
            })
        }
        Err(RiftError::Authentication) => {
            state.clear_session()?;
            setup_bootstrap(&app)
        }
        Err(RiftError::Network(_)) => Ok(BootstrapResult {
            saved_profile: read_profile(&app)?,
            directory: read_room_cache(&app)?,
            requires_authentication: false,
        }),
        Err(error) => Err(error.into()),
    }
}

/// Authenticate to Rift, cache sanitized room data, and retain tokens natively.
#[tauri::command]
pub async fn connect_rift(
    app: AppHandle,
    state: State<'_, AppState>,
    input: RiftConnectionInput,
) -> Result<RoomDirectorySnapshot, CommandError> {
    let session = rift::login(&input).await?;
    let directory = rift::fetch_room_directory(&session).await?;
    write_profile(&app, &rift::profile_for(&session))?;
    write_room_cache(&app, &directory)?;
    state.set_session(session)?;
    Ok(directory)
}

/// Refresh room summaries through the authenticated native session.
#[tauri::command]
pub async fn get_room_directory(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<RoomDirectorySnapshot, CommandError> {
    let session = state.session()?.ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::ConnectionRequired,
            "Connect to Rift before refreshing rooms.",
        )
    })?;

    match rift::fetch_room_directory(&session).await {
        Ok(directory) => {
            write_room_cache(&app, &directory)?;
            Ok(directory)
        }
        Err(RiftError::Network(_)) => read_room_cache(&app)?.ok_or_else(|| {
            CommandError::new(
                CommandErrorKind::Network,
                "Henosis could not reach Rift and has no cached room directory.",
            )
        }),
        Err(RiftError::Authentication) => {
            state.clear_session()?;
            Err(CommandError::new(
                CommandErrorKind::Authentication,
                "Your Rift session expired. Sign in again to continue.",
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Complete one generation-scoped room open through an injected native transport.
async fn open_room_with<T: RoomCommandTransport, R: RoomOpenRuntime>(
    runtime: &R,
    state: &AppState,
    lease: RoomLease,
    room: RoomSummary,
    transport: &T,
) -> Result<RoomConversationSnapshot, CommandError> {
    let result = async {
        let permissions =
            await_room_request(&lease, transport.permissions(&room.server_id)).await?;
        let reconciliation =
            reconcile_open_room(transport, lease.room_id(), None, lease.cancellation())
                .await
                .map_err(reconcile_command_error)?;
        let read_cursor = runtime.read_cursor(&lease)?;
        let page = reconciliation.page;
        state.install_room_page(&lease, page.clone())?;
        let gateway = runtime
            .spawn_gateway(lease.clone(), vec![room.server_id], page.messages.last())
            .map_err(gateway_command_error)?;
        state.install_room_gateway(&lease, gateway)?;
        let boundary = state.seal_room_snapshot_boundary(&lease)?;
        let unread_boundary = unread_boundary(read_cursor.as_deref(), &boundary.page.messages);

        Ok(RoomConversationSnapshot {
            room_id: lease.room_id().to_owned(),
            stream_id: boundary.stream_id,
            last_event_sequence: boundary.last_event_sequence,
            current_user_id: lease.session().gateway_user_id(),
            permissions,
            unread_boundary,
            page: boundary.page,
            connection_status: boundary.connection_status,
        })
    }
    .await;

    if result.is_err() && state.abort_room_open(&lease).is_err() {
        tracing::warn!(
            phase = "open-abort",
            "Henosis could not clear a failed room open"
        );
    }
    result
}

/// Open one cached room through native permissions, bounded history, and one gateway owner.
#[tauri::command]
pub async fn open_room(
    app: AppHandle,
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
) -> Result<RoomConversationSnapshot, CommandError> {
    let session = state.session()?.ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::ConnectionRequired,
            "Connect to Rift before opening a room.",
        )
    })?;
    validate_opaque_id(&room_id, "Choose a valid room before opening it.")?;
    let (lease, room) = begin_resolved_room_open(&state, &session, &room_id, &stream_id, || {
        cached_room_for_session(&app, &session, &room_id)
    })?;
    open_room_with(&app, &state, lease, room, &session).await
}

/// Fetch and merge one explicit older page for the current room generation.
async fn load_older_messages_with<T: RoomCommandTransport>(
    state: &AppState,
    lease: &RoomLease,
    transport: &T,
    before_message_id: &str,
) -> Result<RoomConversationCommandResult<MessagePage>, CommandError> {
    let oldest_message_id = state.oldest_room_message_id(lease)?.ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::Validation,
            "The open room has no loaded history cursor.",
        )
    })?;
    if oldest_message_id != before_message_id {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Load older messages from the current oldest visible message.",
        ));
    }
    let page =
        await_room_request(lease, transport.older(lease.room_id(), before_message_id)).await?;
    state.merge_older_room_page(lease, before_message_id, page)
}

/// Load one bounded page immediately before the current oldest native message.
#[tauri::command]
pub async fn load_older_messages(
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
    before_message_id: String,
) -> Result<RoomConversationCommandResult<MessagePage>, CommandError> {
    validate_opaque_id(&room_id, "Choose a valid open room before loading history.")?;
    validate_opaque_id(
        &before_message_id,
        "Choose a valid loaded message before loading older history.",
    )?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    let session = lease.session().clone();
    load_older_messages_with(&state, &lease, &session, &before_message_id).await
}

/// Create and install one room message through an injected native transport.
async fn send_room_message_with<T: RoomCommandTransport>(
    state: &AppState,
    lease: &RoomLease,
    transport: &T,
    content: &str,
    pending_upload_ids: &[String],
) -> Result<RoomConversationCommandResult<Option<RoomMessage>>, CommandError> {
    validate_send_input(content, pending_upload_ids)?;
    let message = await_room_request(
        lease,
        transport.create(lease.room_id(), content, pending_upload_ids),
    )
    .await?;
    state.apply_room_message(lease, message)
}

/// Send text and zero or more server-staged upload identifiers to the current room.
#[tauri::command]
pub async fn send_room_message(
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
    content: String,
    pending_upload_ids: Vec<String>,
) -> Result<RoomConversationCommandResult<Option<RoomMessage>>, CommandError> {
    validate_opaque_id(
        &room_id,
        "Choose a valid open room before sending a message.",
    )?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    let session = lease.session().clone();
    send_room_message_with(&state, &lease, &session, &content, &pending_upload_ids).await
}

/// Validate, edit, and install one currently loaded room message.
async fn edit_room_message_with<T: RoomCommandTransport>(
    state: &AppState,
    lease: &RoomLease,
    transport: &T,
    message_id: &str,
    content: &str,
) -> Result<RoomConversationCommandResult<Option<RoomMessage>>, CommandError> {
    if content.trim().is_empty() {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Edited room messages cannot be empty.",
        ));
    }
    if state.room_message(lease, message_id)?.is_none() {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Edit a message that is currently loaded in the open room.",
        ));
    }
    let message =
        await_room_request(lease, transport.edit(lease.room_id(), message_id, content)).await?;
    if message.id != message_id {
        return Err(CommandError::new(
            CommandErrorKind::Protocol,
            "Rift returned a different message than the one being edited.",
        ));
    }
    state.apply_room_message(lease, message)
}

/// Edit one currently loaded message in the active room.
#[tauri::command]
pub async fn edit_room_message(
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
    message_id: String,
    content: String,
) -> Result<RoomConversationCommandResult<Option<RoomMessage>>, CommandError> {
    validate_opaque_id(
        &room_id,
        "Choose a valid open room before editing a message.",
    )?;
    validate_opaque_id(&message_id, "Choose a valid loaded message to edit.")?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    let session = lease.session().clone();
    edit_room_message_with(&state, &lease, &session, &message_id, &content).await
}

/// Validate, delete, and remove one currently loaded room message.
async fn delete_room_message_with<T: RoomCommandTransport>(
    state: &AppState,
    lease: &RoomLease,
    transport: &T,
    message_id: &str,
) -> Result<RoomConversationCommandResult<String>, CommandError> {
    if state.room_message(lease, message_id)?.is_none() {
        return Err(CommandError::new(
            CommandErrorKind::Validation,
            "Delete a message that is currently loaded in the open room.",
        ));
    }
    await_room_request(lease, transport.delete(lease.room_id(), message_id)).await?;
    state.delete_room_message(lease, message_id)
}

/// Delete one currently loaded message from the active room.
#[tauri::command]
pub async fn delete_room_message(
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
    message_id: String,
) -> Result<RoomConversationCommandResult<String>, CommandError> {
    validate_opaque_id(
        &room_id,
        "Choose a valid open room before deleting a message.",
    )?;
    validate_opaque_id(&message_id, "Choose a valid loaded message to delete.")?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    let session = lease.session().clone();
    delete_room_message_with(&state, &lease, &session, &message_id).await
}

/// Pick, validate, upload, and report one native attachment selection.
async fn select_and_upload_room_attachments_with<
    T: RoomCommandTransport,
    P: RoomAttachmentPicker,
    E: RoomCommandEventSink,
>(
    state: &AppState,
    lease: &RoomLease,
    transport: &T,
    picker: &P,
    sink: &E,
) -> Result<RoomConversationCommandResult<Vec<PendingRoomAttachment>>, CommandError> {
    let selection = tokio::select! {
        biased;
        _ = lease.cancellation().cancelled() => return Err(stale_room_error()),
        selection = picker.pick() => selection,
    };
    let Some(paths) = selection else {
        return state.sequence_room_command_result(lease, Vec::new());
    };
    state.active_room_page(lease)?;
    let selected_metadata = validate_selected_uploads(&paths)?;
    let transfer_ids = selected_metadata
        .iter()
        .map(|_| next_transfer_id())
        .collect::<Vec<_>>();
    for ((filename, total_bytes), transfer_id) in selected_metadata.iter().zip(&transfer_ids) {
        state.active_room_page(lease)?;
        emit_command_room_event(
            state,
            lease,
            sink,
            RoomConversationEvent::UploadProgress {
                room_id: lease.room_id().to_owned(),
                transfer_id: transfer_id.clone(),
                filename: filename.clone(),
                bytes_sent: 0,
                total_bytes: *total_bytes,
            },
        )?;
    }

    let pending = await_room_request(lease, transport.upload(&paths)).await?;
    state.active_room_page(lease)?;
    if pending.len() != selected_metadata.len() {
        return Err(CommandError::new(
            CommandErrorKind::Protocol,
            "Rift returned incomplete attachment metadata.",
        ));
    }
    for (((filename, total_bytes), transfer_id), attachment) in
        selected_metadata.iter().zip(&transfer_ids).zip(&pending)
    {
        if attachment.filename != *filename || attachment.size_bytes != *total_bytes {
            return Err(CommandError::new(
                CommandErrorKind::Protocol,
                "Rift returned attachment metadata outside the selected upload contract.",
            ));
        }
        state.active_room_page(lease)?;
        emit_command_room_event(
            state,
            lease,
            sink,
            RoomConversationEvent::UploadProgress {
                room_id: lease.room_id().to_owned(),
                transfer_id: transfer_id.clone(),
                filename: filename.clone(),
                bytes_sent: *total_bytes,
                total_bytes: *total_bytes,
            },
        )?;
    }
    state.sequence_room_command_result(lease, pending)
}

/// Open the native file picker and stage bounded attachments without exposing their paths.
#[tauri::command]
pub async fn select_and_upload_room_attachments(
    app: AppHandle,
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
) -> Result<RoomConversationCommandResult<Vec<PendingRoomAttachment>>, CommandError> {
    validate_opaque_id(&room_id, "Choose a valid open room before uploading files.")?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    let session = lease.session().clone();
    select_and_upload_room_attachments_with(
        &state,
        &lease,
        &session,
        &NativeRoomAttachmentPicker,
        &app,
    )
    .await
}

/// Queue one coalesced typing signal through the current gateway actor.
#[tauri::command]
pub async fn send_room_typing(
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
) -> Result<(), CommandError> {
    validate_opaque_id(
        &room_id,
        "Choose a valid open room before sending typing status.",
    )?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    state.send_room_typing(&lease)
}

/// Persist one loaded read marker while its generation and message ordering remain locked.
fn mark_room_read_with<S: RoomReadMarkerStore>(
    state: &AppState,
    lease: &RoomLease,
    message_id: &str,
    store: &S,
    read_at: chrono::DateTime<Utc>,
) -> Result<(), CommandError> {
    let _marker_guard = ROOM_READ_MARKER_LOCK.lock().map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not update native room history.",
        )
    })?;
    let (rift_origin, user_id) = marker_scope(lease)?;
    state.commit_room_read_marker(lease, message_id, |messages, candidate_index| {
        let mut markers = store.read_markers()?;
        let existing_index = markers.iter().position(|marker| {
            marker.rift_origin == rift_origin
                && marker.user_id == user_id
                && marker.room_id == lease.room_id()
        });
        if let Some(existing_message_index) = existing_index.and_then(|index| {
            messages
                .iter()
                .position(|message| message.id == markers[index].last_read_message_id)
        }) && candidate_index <= existing_message_index
        {
            return Ok(());
        }
        if let Some(index) = existing_index {
            markers.remove(index);
        }
        markers.push(RoomReadMarker {
            rift_origin,
            user_id,
            room_id: lease.room_id().to_owned(),
            last_read_message_id: message_id.to_owned(),
            read_at,
        });
        store.write_markers(&markers)
    })
}

/// Persist a loaded message as read unless a newer loaded marker already exists.
#[tauri::command]
pub async fn mark_room_read(
    app: AppHandle,
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
    message_id: String,
) -> Result<(), CommandError> {
    validate_opaque_id(&room_id, "Choose a valid open room before marking it read.")?;
    validate_opaque_id(&message_id, "Choose a valid loaded message to mark read.")?;
    let lease = state.room_operation(&room_id, &stream_id)?;
    mark_room_read_with(&state, &lease, &message_id, &app, Utc::now())
}

/// Validate and close only the exact room stream named by the caller.
fn close_room_with(state: &AppState, room_id: &str, stream_id: &str) -> Result<(), CommandError> {
    validate_opaque_id(room_id, "Choose a valid open room before closing it.")?;
    state.cancel_room_stream(room_id, stream_id)
}

/// Cancel and remove the exact current room generation.
#[tauri::command]
pub async fn close_room(
    state: State<'_, AppState>,
    room_id: String,
    stream_id: String,
) -> Result<(), CommandError> {
    close_room_with(&state, &room_id, &stream_id)
}

/// Clear native Rift tokens and end the remote refresh session when reachable.
#[tauri::command]
pub async fn disconnect_rift(state: State<'_, AppState>) -> Result<(), CommandError> {
    let session = state.session()?;
    state.clear_session()?;
    if let Some(session) = session {
        rift::logout(&session).await;
    }
    Ok(())
}

#[cfg(test)]
/// Deterministic command delegation, validation, and generation-safety tests.
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use reqwest::StatusCode;
    use tokio::sync::Notify;
    use url::Url;

    use super::*;

    /// Records transport calls and returns deterministic sanitized room data.
    struct FakeRoomTransport {
        /// Ordered transport operations that were actually polled.
        calls: Arc<Mutex<Vec<String>>>,
        /// Whether create should return a server-controlled secret-bearing body.
        fail_create: bool,
        /// Optional barrier that leaves a polled create request pending until released.
        create_gate: Option<FakeCreateGate>,
    }

    /// Notifications that make an in-flight create request deterministic in cancellation tests.
    #[derive(Clone)]
    struct FakeCreateGate {
        /// Released after the fake create future records that it was polled.
        started: Arc<Notify>,
        /// Released only when a test deliberately lets the fake request finish.
        release: Arc<Notify>,
    }

    /// Fake transport construction and inspection helpers.
    impl FakeRoomTransport {
        /// Construct a successful fake transport with no observed work.
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_create: false,
                create_gate: None,
            }
        }

        /// Construct a fake whose create response exercises safe error conversion.
        fn failing_create() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_create: true,
                create_gate: None,
            }
        }

        /// Construct a fake whose create request remains pending after its first poll.
        fn blocking_create() -> (Self, Arc<Notify>) {
            let started = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            (
                Self {
                    calls: Arc::new(Mutex::new(Vec::new())),
                    fail_create: false,
                    create_gate: Some(FakeCreateGate {
                        started: Arc::clone(&started),
                        release,
                    }),
                },
                started,
            )
        }

        /// Clone the exact ordered transport calls observed so far.
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .expect("fake transport calls must remain readable")
                .clone()
        }
    }

    /// Supply the newest and forward pages required by the reconciliation supertrait.
    impl MessagePageSource for FakeRoomTransport {
        /// Return one deterministic newest page.
        fn latest<'a>(
            &'a self,
            room_id: &'a str,
            _limit: u32,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            let room_id = room_id.to_owned();
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(format!("latest:{room_id}"));
                Ok(message_page(&room_id, &["message-2", "message-3"], true))
            })
        }

        /// Return no reconnect messages for command-only tests.
        fn after<'a>(
            &'a self,
            room_id: &'a str,
            after_message_id: &'a str,
            _limit: u32,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            let call = format!("after:{room_id}:{after_message_id}");
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(call);
                Ok(MessagePage {
                    messages: Vec::new(),
                    has_older: false,
                })
            })
        }
    }

    /// Implement every narrow command transport operation without network access.
    impl RoomCommandTransport for FakeRoomTransport {
        /// Return a complete safe permission set.
        fn permissions<'a>(
            &'a self,
            server_id: &'a str,
        ) -> BoxFuture<'a, Result<RoomPermissions, RiftError>> {
            let server_id = server_id.to_owned();
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(format!("permissions:{server_id}"));
                Ok(RoomPermissions {
                    send_messages: true,
                    attach_files: true,
                    manage_messages: false,
                    manage_server: false,
                })
            })
        }

        /// Return one older message for explicit pagination tests.
        fn older<'a>(
            &'a self,
            room_id: &'a str,
            before_message_id: &'a str,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            let room_id = room_id.to_owned();
            let call = format!("older:{room_id}:{before_message_id}");
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(call);
                Ok(message_page(&room_id, &["message-1"], false))
            })
        }

        /// Return a created message or one deliberately unsafe upstream error body.
        fn create<'a>(
            &'a self,
            room_id: &'a str,
            content: &'a str,
            pending_upload_ids: &'a [String],
        ) -> BoxFuture<'a, Result<RoomMessage, RiftError>> {
            let room_id = room_id.to_owned();
            let content = content.to_owned();
            let upload_count = pending_upload_ids.len();
            let calls = Arc::clone(&self.calls);
            let fail_create = self.fail_create;
            let create_gate = self.create_gate.clone();
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(format!("create:{room_id}:{upload_count}"));
                if let Some(create_gate) = create_gate {
                    create_gate.started.notify_one();
                    create_gate.release.notified().await;
                }
                if fail_create {
                    return Err(RiftError::Remote {
                        status: StatusCode::BAD_REQUEST,
                        message: "token=server-secret /home/private/file".into(),
                    });
                }
                let mut message = room_message("message-sent", &room_id, 4);
                message.content = content;
                Ok(message)
            })
        }

        /// Return the requested edited message identity.
        fn edit<'a>(
            &'a self,
            room_id: &'a str,
            message_id: &'a str,
            content: &'a str,
        ) -> BoxFuture<'a, Result<RoomMessage, RiftError>> {
            let room_id = room_id.to_owned();
            let message_id = message_id.to_owned();
            let content = content.to_owned();
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(format!("edit:{room_id}:{message_id}"));
                let mut message = room_message(&message_id, &room_id, 5);
                message.content = content;
                message.edited_at = Some("2026-08-03T12:05:00Z".into());
                Ok(message)
            })
        }

        /// Acknowledge one deterministic message deletion.
        fn delete<'a>(
            &'a self,
            room_id: &'a str,
            message_id: &'a str,
        ) -> BoxFuture<'a, Result<(), RiftError>> {
            let call = format!("delete:{room_id}:{message_id}");
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(call);
                Ok(())
            })
        }

        /// Return path-free pending metadata matching each validated selected file.
        fn upload<'a>(
            &'a self,
            paths: &'a [PathBuf],
        ) -> BoxFuture<'a, Result<Vec<PendingRoomAttachment>, RiftError>> {
            let paths = paths.to_vec();
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls
                    .lock()
                    .expect("fake calls must remain writable")
                    .push(format!("upload:{}", paths.len()));
                paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| {
                        Ok(PendingRoomAttachment {
                            upload_id: format!("pending-{index}"),
                            filename: path
                                .file_name()
                                .and_then(|name| name.to_str())
                                .ok_or(RiftError::ProtocolContract)?
                                .to_owned(),
                            content_type: None,
                            size_bytes: std::fs::metadata(path).map_err(RiftError::FileRead)?.len(),
                        })
                    })
                    .collect()
            })
        }
    }

    /// Supplies one counted inert gateway and no cached read cursor to open-room tests.
    #[derive(Default)]
    struct FakeRoomOpenRuntime {
        /// Number of generation-owned gateway actors requested by room opening.
        gateway_spawns: AtomicUsize,
    }

    /// Inspect exact room-open gateway ownership without exposing actor internals.
    impl FakeRoomOpenRuntime {
        /// Return the number of gateway actors requested so far.
        fn gateway_spawn_count(&self) -> usize {
            self.gateway_spawns.load(Ordering::Acquire)
        }
    }

    /// Complete open-room native setup without filesystem or Tauri runtime dependencies.
    impl RoomOpenRuntime for FakeRoomOpenRuntime {
        /// Return no previously persisted read cursor.
        fn read_cursor(&self, _lease: &RoomLease) -> Result<Option<String>, CommandError> {
            Ok(None)
        }

        /// Return an inert gateway whose start barrier succeeds synchronously.
        fn spawn_gateway(
            &self,
            _lease: RoomLease,
            _server_ids: Vec<String>,
            _last_message: Option<&RoomMessage>,
        ) -> Result<RiftGateway, RiftGatewayError> {
            self.gateway_spawns.fetch_add(1, Ordering::AcqRel);
            Ok(crate::gateway::test_closed_rift_gateway())
        }
    }

    /// In-memory read-marker store that exposes exact write counts.
    struct FakeRoomReadMarkerStore {
        /// Current retained markers.
        markers: Mutex<Vec<RoomReadMarker>>,
        /// Number of atomic replacements requested by command logic.
        writes: AtomicUsize,
    }

    /// Read and replace test markers without touching application data.
    impl RoomReadMarkerStore for FakeRoomReadMarkerStore {
        /// Clone the current retained marker set.
        fn read_markers(&self) -> Result<Vec<RoomReadMarker>, CommandError> {
            Ok(self
                .markers
                .lock()
                .expect("fake marker store must remain readable")
                .clone())
        }

        /// Replace the marker set and record one write.
        fn write_markers(&self, markers: &[RoomReadMarker]) -> Result<(), CommandError> {
            self.writes.fetch_add(1, Ordering::AcqRel);
            *self
                .markers
                .lock()
                .expect("fake marker store must remain writable") = markers.to_vec();
            Ok(())
        }
    }

    /// Returns one scripted native file selection.
    struct FakeRoomAttachmentPicker {
        /// Paths returned once for every picker request.
        selection: Option<Vec<PathBuf>>,
    }

    /// Provide deterministic picker cancellation and selection behavior.
    impl RoomAttachmentPicker for FakeRoomAttachmentPicker {
        /// Clone the scripted selection without opening a native dialog.
        fn pick<'a>(&'a self) -> BoxFuture<'a, Option<Vec<PathBuf>>> {
            let selection = self.selection.clone();
            Box::pin(async move { selection })
        }
    }

    /// Native picker that remains pending until the room stream is cancelled.
    struct BlockingRoomAttachmentPicker {
        /// Notification emitted once command logic begins awaiting the picker.
        started: Arc<Notify>,
    }

    /// Model a native dialog that does not settle on its own.
    impl RoomAttachmentPicker for BlockingRoomAttachmentPicker {
        /// Announce the pending selection and then wait forever unless dropped.
        fn pick<'a>(&'a self) -> BoxFuture<'a, Option<Vec<PathBuf>>> {
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                started.notify_one();
                std::future::pending::<Option<Vec<PathBuf>>>().await
            })
        }
    }

    /// Captures only sanitized command-owned room events.
    #[derive(Default)]
    struct FakeRoomEventSink {
        /// Ordered events emitted by the upload helper.
        events: Mutex<Vec<RoomConversationEventEnvelope>>,
    }

    /// Record sanitized events for exact assertions.
    impl RoomCommandEventSink for FakeRoomEventSink {
        /// Append one event without serializing it through Tauri.
        fn emit_room_event(&self, envelope: &RoomConversationEventEnvelope) -> bool {
            self.events
                .lock()
                .expect("fake events must remain writable")
                .push(envelope.clone());
            true
        }
    }

    /// Rejects an ordered event to exercise stream invalidation after delivery failure.
    struct RejectingRoomEventSink;

    /// Report deterministic native delivery failure without retaining the envelope.
    impl RoomCommandEventSink for RejectingRoomEventSink {
        /// Reject the supplied envelope exactly once.
        fn emit_room_event(&self, _envelope: &RoomConversationEventEnvelope) -> bool {
            false
        }
    }

    /// Construct one stable sanitized room message with an ordered timestamp.
    fn room_message(id: &str, room_id: &str, minute: u32) -> RoomMessage {
        RoomMessage {
            id: id.into(),
            room_id: room_id.into(),
            author_id: "user-1".into(),
            author_username: "tester".into(),
            author_display_name: Some("Test User".into()),
            author_avatar_url: None,
            content: format!("message {minute}"),
            edited_at: None,
            created_at: format!("2026-08-03T12:{minute:02}:00Z"),
            message_type: "user".into(),
            attachments: Vec::new(),
        }
    }

    /// Construct one oldest-first deterministic message page.
    fn message_page(room_id: &str, ids: &[&str], has_older: bool) -> MessagePage {
        MessagePage {
            messages: ids
                .iter()
                .enumerate()
                .map(|(index, id)| room_message(id, room_id, index as u32 + 1))
                .collect(),
            has_older,
        }
    }

    /// Construct the cached room metadata required by the native open command.
    fn room_summary() -> RoomSummary {
        RoomSummary {
            id: "room-1".into(),
            name: "Test room".into(),
            server_id: "server-1".into(),
            server_name: Some("Test server".into()),
            topic: None,
            preview: "Room preview".into(),
            latest_author: None,
            last_activity_at: "2026-08-03T12:03:00Z".into(),
            participants: Vec::new(),
            unread_count: 0,
            status: crate::model::RoomStatus::Quiet,
            active_work: None,
            pending_approvals: 0,
        }
    }

    /// Construct an active room generation containing two loaded messages.
    fn active_room_fixture() -> (AppState, RoomLease) {
        let state = AppState::new();
        let session = rift::AuthenticatedRiftClient::gateway_test_client(
            Url::parse("https://rift.example/").expect("fixture endpoint must parse"),
            "user-1",
        );
        state
            .set_session(session.clone())
            .expect("fixture session must install");
        let lease = state
            .begin_room_open(&session, "room-1", "stream-command-0001")
            .expect("fixture room must open");
        state
            .install_room_page(
                &lease,
                message_page("room-1", &["message-2", "message-3"], true),
            )
            .expect("fixture page must install");
        state
            .install_room_gateway(&lease, crate::gateway::test_closed_rift_gateway())
            .expect("fixture gateway must install");
        state
            .seal_room_snapshot_boundary(&lease)
            .expect("fixture snapshot must seal");
        (state, lease)
    }

    /// Open fetches permissions and exactly one latest page before sealing its stream.
    #[tokio::test]
    async fn room_open_fetches_permissions_and_one_initial_latest_page() {
        let state = AppState::new();
        let session = rift::AuthenticatedRiftClient::gateway_test_client(
            Url::parse("https://rift.example/").expect("fixture endpoint must parse"),
            "user-1",
        );
        state
            .set_session(session.clone())
            .expect("open test session must install");
        let transport = FakeRoomTransport::new();
        let runtime = FakeRoomOpenRuntime::default();

        let lease = state
            .begin_room_open(&session, "room-1", "stream-open-test-0001")
            .expect("open test room must begin");
        let snapshot = open_room_with(&runtime, &state, lease, room_summary(), &transport)
            .await
            .expect("bounded room open must succeed");

        assert_eq!(
            transport.calls(),
            vec!["permissions:server-1", "latest:room-1"]
        );
        assert_eq!(snapshot.room_id, "room-1");
        assert_eq!(snapshot.last_event_sequence, 0);
        assert_eq!(snapshot.page.messages.len(), 2);
        assert!(snapshot.permissions.send_messages);
        assert_eq!(runtime.gateway_spawn_count(), 1);
        assert!(state.room_operation("room-1", &snapshot.stream_id).is_ok());
        close_room_with(&state, "room-1", &snapshot.stream_id)
            .expect("opened room must close by exact stream");
    }

    /// Send delegates exactly once, installs the result, and validates empty input locally.
    #[tokio::test]
    async fn room_send_delegates_and_installs_the_sanitized_result() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();
        let pending_ids = vec!["pending-1".to_owned()];

        let sent = send_room_message_with(&state, &lease, &transport, "hello room", &pending_ids)
            .await
            .expect("valid send must succeed");
        let empty = send_room_message_with(&state, &lease, &transport, "  ", &[]).await;

        assert_eq!(sent.sequence, 1);
        assert_eq!(
            sent.value
                .as_ref()
                .expect("send result must retain its message")
                .id,
            "message-sent"
        );
        assert_eq!(transport.calls(), vec!["create:room-1:1"]);
        assert!(matches!(
            empty,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(
            state
                .active_room_messages(&lease)
                .expect("active messages must remain readable")
                .iter()
                .any(|message| message.id == "message-sent")
        );
    }

    /// Explicit older pagination accepts only the currently loaded oldest cursor.
    #[tokio::test]
    async fn room_older_history_rejects_stale_cursors_before_transport() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();

        let stale = load_older_messages_with(&state, &lease, &transport, "message-stale").await;
        let page = load_older_messages_with(&state, &lease, &transport, "message-2")
            .await
            .expect("current oldest cursor must load");

        assert!(matches!(
            stale,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert_eq!(transport.calls(), vec!["older:room-1:message-2"]);
        assert_eq!(page.sequence, 1);
        assert_eq!(page.value.messages[0].id, "message-1");
        assert_eq!(
            state
                .oldest_room_message_id(&lease)
                .expect("oldest message must remain readable")
                .as_deref(),
            Some("message-1")
        );
    }

    /// Edit and delete require a loaded message before delegating to Rift.
    #[tokio::test]
    async fn room_mutations_delegate_only_for_loaded_messages() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();

        let missing =
            edit_room_message_with(&state, &lease, &transport, "message-missing", "updated").await;
        let edited = edit_room_message_with(&state, &lease, &transport, "message-2", "updated")
            .await
            .expect("loaded edit must succeed");
        delete_room_message_with(&state, &lease, &transport, "message-2")
            .await
            .expect("loaded delete must succeed");

        assert!(matches!(
            missing,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert_eq!(
            edited
                .value
                .as_ref()
                .expect("edit result must retain its message")
                .content,
            "updated"
        );
        assert_eq!(
            transport.calls(),
            vec!["edit:room-1:message-2", "delete:room-1:message-2"]
        );
        assert!(
            state
                .room_message(&lease, "message-2")
                .expect("room lookup must succeed")
                .is_none()
        );
    }

    /// Room replacement cancels old work before its transport future is polled.
    #[tokio::test]
    async fn room_replacement_rejects_stale_work_before_transport() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();
        let session = lease.session().clone();
        let replacement = state
            .begin_room_open(&session, "room-2", "stream-command-0002")
            .expect("replacement room must open");

        let stale = send_room_message_with(&state, &lease, &transport, "late", &[]).await;

        assert!(lease.cancellation().is_cancelled());
        assert!(transport.calls().is_empty());
        assert!(matches!(
            stale,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        state
            .close_room(&replacement)
            .expect("replacement room must close");
        assert!(replacement.cancellation().is_cancelled());
    }

    /// Closing the current stream cancels a transport future after it begins in flight.
    #[tokio::test]
    async fn room_close_cancels_an_in_flight_send_request() {
        let (state, lease) = active_room_fixture();
        let (transport, create_started) = FakeRoomTransport::blocking_create();
        let send = send_room_message_with(&state, &lease, &transport, "pending", &[]);
        tokio::pin!(send);

        tokio::select! {
            _ = create_started.notified() => {}
            result = &mut send => panic!("send completed before close: {result:?}"),
        }
        close_room_with(&state, lease.room_id(), lease.stream_id())
            .expect("exact current stream must close");
        let result = tokio::time::timeout(Duration::from_secs(1), &mut send)
            .await
            .expect("room cancellation must promptly end the in-flight request");

        assert!(matches!(
            result,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert_eq!(transport.calls(), vec!["create:room-1:0"]);
    }

    /// Closing the current stream interrupts a native picker that never returns.
    #[tokio::test]
    async fn room_close_cancels_an_in_flight_attachment_picker() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();
        let sink = FakeRoomEventSink::default();
        let picker_started = Arc::new(Notify::new());
        let picker = BlockingRoomAttachmentPicker {
            started: Arc::clone(&picker_started),
        };
        let upload =
            select_and_upload_room_attachments_with(&state, &lease, &transport, &picker, &sink);
        tokio::pin!(upload);

        tokio::select! {
            _ = picker_started.notified() => {}
            result = &mut upload => panic!("upload completed before close: {result:?}"),
        }
        close_room_with(&state, lease.room_id(), lease.stream_id())
            .expect("exact current stream must close");
        let result = tokio::time::timeout(Duration::from_secs(1), &mut upload)
            .await
            .expect("room cancellation must promptly interrupt the native picker");

        assert!(matches!(
            result,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(transport.calls().is_empty());
        assert!(
            sink.events
                .lock()
                .expect("events must remain readable")
                .is_empty()
        );
    }

    /// Cached-room rejection leaves the currently valid native stream untouched.
    #[test]
    fn room_open_rejection_does_not_cancel_the_current_stream() {
        let (state, lease) = active_room_fixture();
        let session = lease.session().clone();

        let rejected = begin_resolved_room_open(
            &state,
            &session,
            "room-missing",
            "stream-missing-0001",
            || {
                Err(CommandError::new(
                    CommandErrorKind::Validation,
                    "That room is no longer in the current Rift directory.",
                ))
            },
        );

        assert!(matches!(
            rejected,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(!lease.cancellation().is_cancelled());
        assert!(
            state
                .room_operation(lease.room_id(), lease.stream_id())
                .is_ok()
        );
    }

    /// A stale same-room stream cannot close the newer generation it resembles.
    #[test]
    fn room_close_rejects_a_stale_same_room_stream() {
        let (state, lease) = active_room_fixture();
        let session = lease.session().clone();
        let replacement = state
            .begin_room_open(&session, lease.room_id(), "stream-command-0002")
            .expect("same room replacement must begin");

        let stale_close = close_room_with(&state, lease.room_id(), lease.stream_id());

        assert!(matches!(
            stale_close,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(!replacement.cancellation().is_cancelled());
        state
            .abort_room_open(&replacement)
            .expect("replacement cleanup must succeed");
    }

    /// Marking an older loaded message never regresses a newer retained read marker.
    #[test]
    fn room_read_marker_ignores_an_older_loaded_candidate() {
        let (state, lease) = active_room_fixture();
        let (rift_origin, user_id) = marker_scope(&lease).expect("marker scope must resolve");
        let existing_read_at = "2026-08-03T12:03:00Z"
            .parse()
            .expect("existing read time must parse");
        let store = FakeRoomReadMarkerStore {
            markers: Mutex::new(vec![RoomReadMarker {
                rift_origin,
                user_id,
                room_id: lease.room_id().to_owned(),
                last_read_message_id: "message-3".into(),
                read_at: existing_read_at,
            }]),
            writes: AtomicUsize::new(0),
        };
        let attempted_read_at = "2026-08-03T12:04:00Z"
            .parse()
            .expect("attempted read time must parse");

        mark_room_read_with(&state, &lease, "message-2", &store, attempted_read_at)
            .expect("older loaded marker must be a harmless no-op");

        assert_eq!(store.writes.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .markers
                .lock()
                .expect("fake marker must remain readable")[0]
                .last_read_message_id,
            "message-3"
        );
    }

    /// Failed ordered delivery cancels the stream so no later command can skip the gap.
    #[test]
    fn room_event_delivery_failure_invalidates_its_stream() {
        let (state, lease) = active_room_fixture();

        let failed = emit_command_room_event(
            &state,
            &lease,
            &RejectingRoomEventSink,
            RoomConversationEvent::TypingStart {
                room_id: lease.room_id().to_owned(),
                user_id: "user-2".into(),
                username: "collaborator".into(),
            },
        );

        assert!(matches!(
            failed,
            Err(CommandError {
                kind: CommandErrorKind::Protocol,
                ..
            })
        ));
        assert!(lease.cancellation().is_cancelled());
        assert!(
            state
                .room_operation(lease.room_id(), lease.stream_id())
                .is_err()
        );
    }

    /// Picker cancellation and oversized selections never reach upload transport.
    #[tokio::test]
    async fn room_upload_validates_selection_before_transport() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();
        let sink = FakeRoomEventSink::default();
        let cancelled_picker = FakeRoomAttachmentPicker { selection: None };
        let oversized_picker = FakeRoomAttachmentPicker {
            selection: Some(
                (0..=rift::MAX_NATIVE_UPLOAD_FILES)
                    .map(|index| PathBuf::from(format!("missing-{index}.bin")))
                    .collect(),
            ),
        };

        let cancelled = select_and_upload_room_attachments_with(
            &state,
            &lease,
            &transport,
            &cancelled_picker,
            &sink,
        )
        .await
        .expect("picker cancellation must be harmless");
        let oversized = select_and_upload_room_attachments_with(
            &state,
            &lease,
            &transport,
            &oversized_picker,
            &sink,
        )
        .await;

        assert!(cancelled.value.is_empty());
        assert!(matches!(
            oversized,
            Err(CommandError {
                kind: CommandErrorKind::Validation,
                ..
            })
        ));
        assert!(transport.calls().is_empty());
        assert!(
            sink.events
                .lock()
                .expect("events must remain readable")
                .is_empty()
        );
    }

    /// Successful upload events and returned metadata never contain the native directory.
    #[tokio::test]
    async fn room_upload_emits_path_free_progress() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::new();
        let sink = FakeRoomEventSink::default();
        let selected = tempfile::tempdir().expect("upload fixture directory must exist");
        let selected_path = selected.path().join("evidence.txt");
        std::fs::write(&selected_path, b"native-only").expect("upload fixture must write");
        let picker = FakeRoomAttachmentPicker {
            selection: Some(vec![selected_path]),
        };

        let pending =
            select_and_upload_room_attachments_with(&state, &lease, &transport, &picker, &sink)
                .await
                .expect("valid native upload must succeed");
        let serialized_events =
            serde_json::to_string(&*sink.events.lock().expect("events must remain readable"))
                .expect("sanitized events must serialize");
        let serialized_pending =
            serde_json::to_string(&pending).expect("pending metadata must serialize");
        let native_directory = selected.path().to_string_lossy();

        assert_eq!(transport.calls(), vec!["upload:1"]);
        assert_eq!(pending.value[0].filename, "evidence.txt");
        assert!(!serialized_events.contains(native_directory.as_ref()));
        assert!(!serialized_pending.contains(native_directory.as_ref()));
        assert_eq!(
            sink.events
                .lock()
                .expect("events must remain readable")
                .len(),
            2
        );
    }

    /// Server-controlled error bodies never cross the serialized command boundary.
    #[tokio::test]
    async fn room_command_errors_serialize_without_secrets_or_paths() {
        let (state, lease) = active_room_fixture();
        let transport = FakeRoomTransport::failing_create();

        let error = send_room_message_with(&state, &lease, &transport, "hello", &[])
            .await
            .expect_err("unsafe upstream error must fail safely");
        let serialized = serde_json::to_string(&error).expect("command error must serialize");

        assert!(matches!(error.kind, CommandErrorKind::Protocol));
        assert!(!serialized.contains("server-secret"));
        assert!(!serialized.contains("/home/private/file"));
        assert_eq!(transport.calls(), vec!["create:room-1:0"]);
    }
}
