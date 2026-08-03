//! Bounded native reconciliation for the currently open Rift room.

use std::collections::HashSet;

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::model::{MessagePage, RoomMessage, RoomUnreadBoundary};
use crate::rift::{self, AuthenticatedRiftClient, RiftError};

/// Maximum number of forward pages accepted before replacing the live window.
pub(crate) const MAX_RECONCILE_PAGES: usize = 5;

/// Message count requested for every bounded reconciliation page.
pub(crate) const RECONCILE_PAGE_SIZE: u32 = 100;

/// HTTP boundary used by production reconciliation and deterministic tests.
pub(crate) trait MessagePageSource: Send + Sync {
    /// Fetch the newest bounded page for one room.
    fn latest<'a>(
        &'a self,
        room_id: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<MessagePage, RiftError>>;

    /// Fetch one oldest-first page strictly after an opaque message cursor.
    fn after<'a>(
        &'a self,
        room_id: &'a str,
        after_message_id: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<MessagePage, RiftError>>;
}

/// Use the authenticated native Rift client as the production page source.
impl MessagePageSource for AuthenticatedRiftClient {
    /// Delegate newest-page requests to the native authenticated transport.
    fn latest<'a>(
        &'a self,
        room_id: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
        Box::pin(rift::latest_messages(self, room_id, limit))
    }

    /// Delegate forward-page requests to the native authenticated transport.
    fn after<'a>(
        &'a self,
        room_id: &'a str,
        after_message_id: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
        Box::pin(rift::messages_after(self, room_id, after_message_id, limit))
    }
}

/// Sanitized output from one initial-open or reconnect reconciliation pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Reconciliation {
    /// Oldest-first page to install or merge into the current conversation.
    pub(crate) page: MessagePage,
    /// Whether an incomplete forward walk requires replacing the live window.
    pub(crate) replace_live_window: bool,
}

/// Reconciliation failure that distinguishes cancellation from Rift failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReconcileError {
    /// The owning room generation was cancelled before a result could emit.
    #[error("room reconciliation was cancelled")]
    Cancelled,
    /// The native Rift request or its sanitized response failed.
    #[error(transparent)]
    Rift(#[from] RiftError),
}

/// Fetch one newest page or bounded forward pages for the current room.
pub(crate) async fn reconcile_open_room(
    source: &dyn MessagePageSource,
    room_id: &str,
    after_cursor: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<Reconciliation, ReconcileError> {
    validate_request_identifiers(room_id, after_cursor)?;

    let Some(mut cursor) = after_cursor.map(str::to_owned) else {
        let page = fetch_latest_window(source, room_id, cancellation).await?;
        return Ok(Reconciliation {
            page,
            replace_live_window: false,
        });
    };

    let mut messages = Vec::new();
    let mut seen_message_ids = HashSet::from([cursor.clone()]);

    for _ in 0..MAX_RECONCILE_PAGES {
        let page = match await_request(cancellation, || {
            source.after(room_id, &cursor, RECONCILE_PAGE_SIZE)
        })
        .await
        {
            Ok(page) => page,
            Err(ReconcileError::Rift(RiftError::InvalidMessageCursor)) => {
                return latest_fallback(source, room_id, cancellation).await;
            }
            Err(error) => return Err(error),
        };

        let raw_message_count = page.messages.len();
        if raw_message_count > RECONCILE_PAGE_SIZE as usize {
            return Err(RiftError::ProtocolContract.into());
        }
        let next_cursor = page.messages.last().map(|message| message.id.clone());
        let page = validate_and_deduplicate_page(room_id, page, &mut seen_message_ids)?;
        messages.extend(page.messages);

        if raw_message_count < RECONCILE_PAGE_SIZE as usize {
            return Ok(Reconciliation {
                page: MessagePage {
                    messages,
                    has_older: false,
                },
                replace_live_window: false,
            });
        }

        cursor = next_cursor.ok_or(RiftError::ProtocolContract)?;
    }

    latest_fallback(source, room_id, cancellation).await
}

/// Derive the bounded unread-divider placement from one loaded read cursor.
pub(crate) fn unread_boundary(
    read_cursor: Option<&str>,
    messages: &[RoomMessage],
) -> RoomUnreadBoundary {
    let Some(read_cursor) = read_cursor else {
        return RoomUnreadBoundary::None;
    };
    if messages.is_empty() {
        return RoomUnreadBoundary::None;
    }

    match messages
        .iter()
        .position(|message| message.id == read_cursor)
    {
        Some(index) if index + 1 < messages.len() => RoomUnreadBoundary::BeforeMessage {
            message_id: messages[index + 1].id.clone(),
        },
        Some(_) => RoomUnreadBoundary::None,
        None => RoomUnreadBoundary::BeforeLoadedWindow,
    }
}

/// Reject empty request identifiers before issuing any native HTTP request.
fn validate_request_identifiers(
    room_id: &str,
    after_cursor: Option<&str>,
) -> Result<(), ReconcileError> {
    if room_id.is_empty() {
        return Err(RiftError::Validation("Rift room identifiers cannot be empty.".into()).into());
    }
    if after_cursor.is_some_and(str::is_empty) {
        return Err(RiftError::Validation("Rift message cursors cannot be empty.".into()).into());
    }
    Ok(())
}

/// Fetch and validate one newest page while making cancellation interruptible.
async fn fetch_latest_window(
    source: &dyn MessagePageSource,
    room_id: &str,
    cancellation: &CancellationToken,
) -> Result<MessagePage, ReconcileError> {
    let page = await_request(cancellation, || source.latest(room_id, RECONCILE_PAGE_SIZE)).await?;
    if page.messages.len() > RECONCILE_PAGE_SIZE as usize {
        return Err(RiftError::ProtocolContract.into());
    }
    validate_and_deduplicate_page(room_id, page, &mut HashSet::new()).map_err(Into::into)
}

/// Replace an incomplete forward walk with exactly one validated newest page.
async fn latest_fallback(
    source: &dyn MessagePageSource,
    room_id: &str,
    cancellation: &CancellationToken,
) -> Result<Reconciliation, ReconcileError> {
    Ok(Reconciliation {
        page: fetch_latest_window(source, room_id, cancellation).await?,
        replace_live_window: true,
    })
}

/// Race one Rift page request against cancellation of its owning room generation.
async fn await_request<'a>(
    cancellation: &CancellationToken,
    request: impl FnOnce() -> BoxFuture<'a, Result<MessagePage, RiftError>>,
) -> Result<MessagePage, ReconcileError> {
    if cancellation.is_cancelled() {
        return Err(ReconcileError::Cancelled);
    }
    let request = request();

    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(ReconcileError::Cancelled),
        result = request => {
            if cancellation.is_cancelled() {
                Err(ReconcileError::Cancelled)
            } else {
                result.map_err(Into::into)
            }
        }
    }
}

/// Validate room bindings and remove duplicate message IDs without partial mutation.
fn validate_and_deduplicate_page(
    room_id: &str,
    page: MessagePage,
    seen_message_ids: &mut HashSet<String>,
) -> Result<MessagePage, RiftError> {
    if page
        .messages
        .iter()
        .any(|message| message.id.is_empty() || message.room_id != room_id)
    {
        return Err(RiftError::ProtocolContract);
    }

    let messages = page
        .messages
        .into_iter()
        .filter(|message| seen_message_ids.insert(message.id.clone()))
        .collect();
    Ok(MessagePage {
        messages,
        has_older: page.has_older,
    })
}

/// Deterministic unit tests for bounded reconciliation behavior.
#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::pending;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::Notify;
    use tokio::time::timeout;

    use super::*;

    /// One observable request made through the test-only page source.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum RecordedRequest {
        /// A newest-page request.
        Latest {
            /// Requested room identifier.
            room_id: String,
            /// Requested page size.
            limit: u32,
        },
        /// A forward-page request.
        After {
            /// Requested room identifier.
            room_id: String,
            /// Opaque cursor supplied to the request.
            cursor: String,
            /// Requested page size.
            limit: u32,
        },
    }

    /// One scripted result returned by the deterministic page source.
    enum ScriptedReply {
        /// Return one successful message page.
        Page(MessagePage),
        /// Return the cursor-specific Rift failure that permits fallback.
        InvalidCursor,
        /// Return a non-cursor protocol failure.
        ProtocolFailure,
        /// Notify the test once polled and then remain pending forever.
        Pending(Arc<Notify>),
    }

    /// Thread-safe scripted page source recording every requested operation.
    struct ScriptedSource {
        /// Ordered replies consumed by latest and after alike.
        replies: Mutex<VecDeque<ScriptedReply>>,
        /// Ordered requests observed before their futures are returned.
        requests: Mutex<Vec<RecordedRequest>>,
    }

    /// Construct and inspect deterministic page-source fixtures.
    impl ScriptedSource {
        /// Create a source that consumes the supplied replies in order.
        fn new(replies: impl IntoIterator<Item = ScriptedReply>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// Clone every request recorded so far.
        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests
                .lock()
                .expect("scripted request lock must remain healthy")
                .clone()
        }

        /// Record one operation and build the future for its next scripted reply.
        fn respond<'a>(
            &'a self,
            request: RecordedRequest,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            self.requests
                .lock()
                .expect("scripted request lock must remain healthy")
                .push(request);
            let reply = self
                .replies
                .lock()
                .expect("scripted reply lock must remain healthy")
                .pop_front()
                .expect("every test request must have a scripted reply");
            match reply {
                ScriptedReply::Page(page) => Box::pin(async move { Ok(page) }),
                ScriptedReply::InvalidCursor => {
                    Box::pin(async { Err(RiftError::InvalidMessageCursor) })
                }
                ScriptedReply::ProtocolFailure => {
                    Box::pin(async { Err(RiftError::ProtocolContract) })
                }
                ScriptedReply::Pending(started) => Box::pin(async move {
                    started.notify_one();
                    pending::<Result<MessagePage, RiftError>>().await
                }),
            }
        }
    }

    /// Supply scripted newest and forward pages without a backward-page method.
    impl MessagePageSource for ScriptedSource {
        /// Record and answer one newest-page request.
        fn latest<'a>(
            &'a self,
            room_id: &'a str,
            limit: u32,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            self.respond(RecordedRequest::Latest {
                room_id: room_id.to_owned(),
                limit,
            })
        }

        /// Record and answer one forward-page request.
        fn after<'a>(
            &'a self,
            room_id: &'a str,
            after_message_id: &'a str,
            limit: u32,
        ) -> BoxFuture<'a, Result<MessagePage, RiftError>> {
            self.respond(RecordedRequest::After {
                room_id: room_id.to_owned(),
                cursor: after_message_id.to_owned(),
                limit,
            })
        }
    }

    /// Build one valid sanitized room message fixture.
    fn message(id: impl Into<String>, room_id: &str) -> RoomMessage {
        let id = id.into();
        RoomMessage {
            id: id.clone(),
            room_id: room_id.into(),
            author_id: "user-1".into(),
            author_username: "operator".into(),
            author_display_name: Some("Operator".into()),
            author_avatar_url: None,
            content: format!("message {id}"),
            edited_at: None,
            created_at: "2026-08-03T12:00:00+00:00".into(),
            message_type: "user".into(),
            attachments: Vec::new(),
        }
    }

    /// Build one page from ordered message identifiers.
    fn page(ids: &[&str], room_id: &str, has_older: bool) -> MessagePage {
        MessagePage {
            messages: ids.iter().map(|id| message(*id, room_id)).collect(),
            has_older,
        }
    }

    /// Build one full reconciliation page with deterministic unique identifiers.
    fn full_page(prefix: &str, room_id: &str) -> MessagePage {
        MessagePage {
            messages: (0..RECONCILE_PAGE_SIZE)
                .map(|index| message(format!("{prefix}-{index:03}"), room_id))
                .collect(),
            has_older: false,
        }
    }

    /// Extract ordered message identifiers from a reconciliation result.
    fn message_ids(reconciliation: &Reconciliation) -> Vec<&str> {
        reconciliation
            .page
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect()
    }

    /// Initial open performs exactly one latest request and deduplicates its page.
    #[tokio::test]
    async fn initial_open_fetches_one_latest_page() {
        let source = ScriptedSource::new([ScriptedReply::Page(page(
            &["message-1", "message-1", "message-2"],
            "room-1",
            true,
        ))]);

        let result = reconcile_open_room(&source, "room-1", None, &CancellationToken::new())
            .await
            .expect("initial reconciliation must succeed");

        assert_eq!(message_ids(&result), vec!["message-1", "message-2"]);
        assert!(result.page.has_older);
        assert!(!result.replace_live_window);
        assert_eq!(
            source.requests(),
            vec![RecordedRequest::Latest {
                room_id: "room-1".into(),
                limit: RECONCILE_PAGE_SIZE,
            }]
        );
    }

    /// Reconnect advances the cursor across full pages and stops on a short page.
    #[tokio::test]
    async fn reconnect_walks_after_pages_and_advances_the_cursor() {
        let first_page = full_page("first", "room-1");
        let first_page_cursor = first_page
            .messages
            .last()
            .expect("full page must have a final message")
            .id
            .clone();
        let source = ScriptedSource::new([
            ScriptedReply::Page(first_page),
            ScriptedReply::Page(page(&["second-1", "second-2"], "room-1", false)),
        ]);

        let result = reconcile_open_room(
            &source,
            "room-1",
            Some("loaded-message"),
            &CancellationToken::new(),
        )
        .await
        .expect("forward reconciliation must succeed");

        assert_eq!(result.page.messages.len(), 102);
        assert!(!result.page.has_older);
        assert!(!result.replace_live_window);
        assert_eq!(
            source.requests(),
            vec![
                RecordedRequest::After {
                    room_id: "room-1".into(),
                    cursor: "loaded-message".into(),
                    limit: RECONCILE_PAGE_SIZE,
                },
                RecordedRequest::After {
                    room_id: "room-1".into(),
                    cursor: first_page_cursor,
                    limit: RECONCILE_PAGE_SIZE,
                },
            ]
        );
    }

    /// Valid empty forward pages stop without a newest-page fallback.
    #[tokio::test]
    async fn valid_empty_after_page_stops_without_fallback() {
        let source = ScriptedSource::new([ScriptedReply::Page(page(&[], "room-1", false))]);

        let result = reconcile_open_room(
            &source,
            "room-1",
            Some("message-1"),
            &CancellationToken::new(),
        )
        .await
        .expect("empty forward page is valid");

        assert!(result.page.messages.is_empty());
        assert!(!result.replace_live_window);
        assert_eq!(source.requests().len(), 1);
        assert!(matches!(
            source.requests().as_slice(),
            [RecordedRequest::After { .. }]
        ));
    }

    /// Duplicate IDs across forward pages are emitted only once.
    #[tokio::test]
    async fn reconnect_deduplicates_ids_across_pages() {
        let first_page = full_page("message", "room-1");
        let duplicate_id = first_page
            .messages
            .last()
            .expect("full page must have a final message")
            .id
            .clone();
        let source = ScriptedSource::new([
            ScriptedReply::Page(first_page),
            ScriptedReply::Page(MessagePage {
                messages: vec![message(duplicate_id, "room-1"), message("new", "room-1")],
                has_older: false,
            }),
        ]);

        let result = reconcile_open_room(
            &source,
            "room-1",
            Some("message-000"),
            &CancellationToken::new(),
        )
        .await
        .expect("duplicate forward messages must be tolerated");

        assert_eq!(result.page.messages.len(), 100);
        assert_eq!(
            result
                .page
                .messages
                .last()
                .map(|message| message.id.as_str()),
            Some("new")
        );
    }

    /// Five full forward pages trigger one latest replacement and no seventh request.
    #[tokio::test]
    async fn five_full_pages_trigger_one_bounded_latest_fallback() {
        let mut replies = (0..MAX_RECONCILE_PAGES)
            .map(|page_index| {
                ScriptedReply::Page(full_page(&format!("page-{page_index}"), "room-1"))
            })
            .collect::<Vec<_>>();
        replies.push(ScriptedReply::Page(page(
            &["latest-1", "latest-1", "latest-2"],
            "room-1",
            true,
        )));
        let source = ScriptedSource::new(replies);

        let result = reconcile_open_room(
            &source,
            "room-1",
            Some("loaded-message"),
            &CancellationToken::new(),
        )
        .await
        .expect("bounded fallback must succeed");

        assert_eq!(message_ids(&result), vec!["latest-1", "latest-2"]);
        assert!(result.page.has_older);
        assert!(result.replace_live_window);
        assert_eq!(source.requests().len(), MAX_RECONCILE_PAGES + 1);
        assert!(matches!(
            source.requests().last(),
            Some(RecordedRequest::Latest { .. })
        ));
    }

    /// Only the typed invalid-cursor error triggers one latest fallback.
    #[tokio::test]
    async fn invalid_cursor_triggers_exactly_one_latest_fallback() {
        let source = ScriptedSource::new([
            ScriptedReply::InvalidCursor,
            ScriptedReply::Page(page(&["latest"], "room-1", false)),
        ]);

        let result = reconcile_open_room(
            &source,
            "room-1",
            Some("deleted-message"),
            &CancellationToken::new(),
        )
        .await
        .expect("invalid cursor must use bounded fallback");

        assert_eq!(message_ids(&result), vec!["latest"]);
        assert!(result.replace_live_window);
        assert_eq!(
            source.requests(),
            vec![
                RecordedRequest::After {
                    room_id: "room-1".into(),
                    cursor: "deleted-message".into(),
                    limit: RECONCILE_PAGE_SIZE,
                },
                RecordedRequest::Latest {
                    room_id: "room-1".into(),
                    limit: RECONCILE_PAGE_SIZE,
                },
            ]
        );
    }

    /// General Rift failures propagate without making a latest request.
    #[tokio::test]
    async fn non_cursor_failure_does_not_fallback() {
        let source = ScriptedSource::new([ScriptedReply::ProtocolFailure]);

        let error = reconcile_open_room(
            &source,
            "room-1",
            Some("message-1"),
            &CancellationToken::new(),
        )
        .await
        .expect_err("protocol failure must propagate");

        assert!(matches!(
            error,
            ReconcileError::Rift(RiftError::ProtocolContract)
        ));
        assert_eq!(source.requests().len(), 1);
    }

    /// Empty request identifiers fail before a page-source method is called.
    #[tokio::test]
    async fn empty_request_identifiers_are_rejected_without_requests() {
        let source = ScriptedSource::new([]);

        let empty_room = reconcile_open_room(&source, "", None, &CancellationToken::new()).await;
        let empty_cursor =
            reconcile_open_room(&source, "room-1", Some(""), &CancellationToken::new()).await;

        assert!(matches!(
            empty_room,
            Err(ReconcileError::Rift(RiftError::Validation(_)))
        ));
        assert!(matches!(
            empty_cursor,
            Err(ReconcileError::Rift(RiftError::Validation(_)))
        ));
        assert!(source.requests().is_empty());
    }

    /// Empty message IDs and cross-room messages reject the complete page.
    #[tokio::test]
    async fn invalid_message_identity_or_room_binding_is_rejected() {
        let empty_id_source =
            ScriptedSource::new([ScriptedReply::Page(page(&[""], "room-1", false))]);
        let wrong_room_source =
            ScriptedSource::new([ScriptedReply::Page(page(&["message-1"], "room-2", false))]);

        let empty_id =
            reconcile_open_room(&empty_id_source, "room-1", None, &CancellationToken::new()).await;
        let wrong_room = reconcile_open_room(
            &wrong_room_source,
            "room-1",
            None,
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            empty_id,
            Err(ReconcileError::Rift(RiftError::ProtocolContract))
        ));
        assert!(matches!(
            wrong_room,
            Err(ReconcileError::Rift(RiftError::ProtocolContract))
        ));
    }

    /// A page exceeding the requested bound is treated as a protocol violation.
    #[tokio::test]
    async fn oversized_page_is_rejected() {
        let source = ScriptedSource::new([ScriptedReply::Page(MessagePage {
            messages: (0..=RECONCILE_PAGE_SIZE)
                .map(|index| message(format!("message-{index}"), "room-1"))
                .collect(),
            has_older: true,
        })]);

        let error = reconcile_open_room(&source, "room-1", None, &CancellationToken::new()).await;

        assert!(matches!(
            error,
            Err(ReconcileError::Rift(RiftError::ProtocolContract))
        ));
    }

    /// Pre-cancelled work returns without constructing any HTTP request.
    #[tokio::test]
    async fn pre_cancelled_reconciliation_makes_no_request() {
        let source = ScriptedSource::new([]);
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = reconcile_open_room(&source, "room-1", None, &cancellation).await;

        assert!(matches!(error, Err(ReconcileError::Cancelled)));
        assert!(source.requests().is_empty());
    }

    /// Cancellation interrupts a pending latest request deterministically.
    #[tokio::test]
    async fn cancellation_interrupts_pending_latest_request() {
        let started = Arc::new(Notify::new());
        let source = Arc::new(ScriptedSource::new([ScriptedReply::Pending(
            started.clone(),
        )]));
        let cancellation = CancellationToken::new();
        let task_source = source.clone();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            reconcile_open_room(task_source.as_ref(), "room-1", None, &task_cancellation).await
        });

        started.notified().await;
        cancellation.cancel();
        let result = timeout(Duration::from_secs(1), task)
            .await
            .expect("cancellation must not leave a request pending")
            .expect("reconciliation task must join");

        assert!(matches!(result, Err(ReconcileError::Cancelled)));
        assert_eq!(source.requests().len(), 1);
    }

    /// Cancellation also interrupts a pending after request.
    #[tokio::test]
    async fn cancellation_interrupts_pending_after_request() {
        let started = Arc::new(Notify::new());
        let source = Arc::new(ScriptedSource::new([ScriptedReply::Pending(
            started.clone(),
        )]));
        let cancellation = CancellationToken::new();
        let task_source = source.clone();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            reconcile_open_room(
                task_source.as_ref(),
                "room-1",
                Some("message-1"),
                &task_cancellation,
            )
            .await
        });

        started.notified().await;
        cancellation.cancel();
        let result = timeout(Duration::from_secs(1), task)
            .await
            .expect("cancellation must not leave a request pending")
            .expect("reconciliation task must join");

        assert!(matches!(result, Err(ReconcileError::Cancelled)));
        assert!(matches!(
            source.requests().as_slice(),
            [RecordedRequest::After { .. }]
        ));
    }

    /// No read cursor produces no divider even when messages are loaded.
    #[test]
    fn unread_boundary_is_none_without_a_read_cursor() {
        let messages = vec![message("message-1", "room-1")];

        assert_eq!(unread_boundary(None, &messages), RoomUnreadBoundary::None);
    }

    /// A loaded read cursor places the divider before the following message.
    #[test]
    fn unread_boundary_targets_first_message_after_loaded_cursor() {
        let messages = vec![
            message("message-1", "room-1"),
            message("message-2", "room-1"),
            message("message-3", "room-1"),
        ];

        assert_eq!(
            unread_boundary(Some("message-1"), &messages),
            RoomUnreadBoundary::BeforeMessage {
                message_id: "message-2".into(),
            }
        );
    }

    /// A newest read cursor or empty loaded page produces no divider.
    #[test]
    fn unread_boundary_is_none_at_newest_or_for_empty_page() {
        let messages = vec![
            message("message-1", "room-1"),
            message("message-2", "room-1"),
        ];

        assert_eq!(
            unread_boundary(Some("message-2"), &messages),
            RoomUnreadBoundary::None
        );
        assert_eq!(
            unread_boundary(Some("message-2"), &[]),
            RoomUnreadBoundary::None
        );
    }

    /// An unavailable read cursor labels the front of a nonempty loaded window.
    #[test]
    fn unread_boundary_marks_before_loaded_window_for_unavailable_cursor() {
        let messages = vec![message("message-2", "room-1")];

        assert_eq!(
            unread_boundary(Some("deleted-message"), &messages),
            RoomUnreadBoundary::BeforeLoadedWindow
        );
    }
}
