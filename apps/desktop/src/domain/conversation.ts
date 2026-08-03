/** Pure, generation-scoped projection of the sanitized native conversation contract. */

/** Effective room capabilities safe for React affordance decisions. */
export interface RoomPermissions {
  /** Whether the signed-in human may create messages. */
  sendMessages: boolean;
  /** Whether the signed-in human may send messages with attachments. */
  attachFiles: boolean;
  /** Whether the signed-in human may delete another member's messages. */
  manageMessages: boolean;
  /** Whether the signed-in human may manage room-wide settings. */
  manageServer: boolean;
}

/** Bounded unread-divider placement supplied by the native client. */
export type RoomUnreadBoundary =
  | {
      /** Discriminator for a conversation without a visible unread divider. */
      kind: "none";
    }
  | {
      /** Discriminator for a divider immediately before one loaded message. */
      kind: "beforeMessage";
      /** First loaded message that has not been marked read. */
      messageId: string;
    }
  | {
      /** Discriminator for unread history preceding the bounded loaded window. */
      kind: "beforeLoadedWindow";
    };

/** Server-staged upload metadata that excludes native file paths. */
export interface PendingRoomAttachment {
  /** Opaque upload identifier supplied when sending the message. */
  uploadId: string;
  /** Original display filename without a local directory. */
  filename: string;
  /** Optional declared media type. */
  contentType: string | null;
  /** Uploaded byte count validated by the native client. */
  sizeBytes: number;
}

/** Sanitized attachment metadata nested beneath one room message. */
export interface RoomAttachment {
  /** Stable attachment identifier. */
  id: string;
  /** Original display filename without a local directory. */
  filename: string;
  /** Same-origin HTTP or HTTPS URL validated by the native client. */
  url: string;
  /** Optional declared media type. */
  contentType: string | null;
  /** Stored byte count when the server supplied one. */
  sizeBytes: number | null;
}

/** Complete sanitized room message shared by snapshots, commands, and events. */
export interface RoomMessage {
  /** Stable message identifier. */
  id: string;
  /** Room containing the message. */
  roomId: string;
  /** Stable author identifier. */
  authorId: string;
  /** Unambiguous author login handle. */
  authorUsername: string;
  /** Optional human-facing author name. */
  authorDisplayName: string | null;
  /** Optional validated author avatar URL. */
  authorAvatarUrl: string | null;
  /** Message body, possibly empty for attachment-only messages. */
  content: string;
  /** ISO timestamp of the latest edit when present. */
  editedAt: string | null;
  /** ISO timestamp when the server created the message. */
  createdAt: string;
  /** Server message discriminator such as user, agent, stimulus, or system. */
  messageType: string;
  /** Sanitized attachments in server response order. */
  attachments: RoomAttachment[];
}

/** One oldest-first bounded page of sanitized room messages. */
export interface MessagePage {
  /** Messages ordered oldest to newest for direct timeline insertion. */
  messages: RoomMessage[];
  /** Whether an explicit user action may request an older page. */
  hasOlder: boolean;
}

/** Native transport state for the currently open room. */
export type RoomConnectionStatus =
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected";

/** Complete sanitized native state returned when opening one room. */
export interface RoomConversationSnapshot {
  /** Open room identifier. */
  roomId: string;
  /** Caller-generated one-use token bound to this room generation. */
  streamId: string;
  /** Highest shared sequence already reflected in this snapshot. */
  lastEventSequence: number;
  /** Signed-in user identifier used for authorship affordances. */
  currentUserId: string;
  /** Server-authoritative capabilities for the signed-in human. */
  permissions: RoomPermissions;
  /** Placement of the bounded unread divider. */
  unreadBoundary: RoomUnreadBoundary;
  /** Initial oldest-first live message window. */
  page: MessagePage;
  /** Current native transport state. */
  connectionStatus: RoomConnectionStatus;
}

/** Sanitized incremental update emitted on the fixed native room event channel. */
export type RoomConversationEvent =
  | {
      /** Event discriminator for a complete message insertion. */
      type: "messageCreate";
      /** Complete message insertion payload. */
      data: {
        /** Room receiving the message. */
        roomId: string;
        /** Complete sanitized message. */
        message: RoomMessage;
      };
    }
  | {
      /** Event discriminator for a partial message edit. */
      type: "messageUpdate";
      /** Partial message edit payload. */
      data: {
        /** Room containing the edited message. */
        roomId: string;
        /** Edited message identifier. */
        messageId: string;
        /** Replacement message body. */
        content: string;
        /** ISO timestamp of the edit. */
        editedAt: string;
      };
    }
  | {
      /** Event discriminator for a message deletion. */
      type: "messageDelete";
      /** Message deletion payload. */
      data: {
        /** Room containing the deleted message. */
        roomId: string;
        /** Deleted message identifier. */
        messageId: string;
      };
    }
  | {
      /** Event discriminator for a short-lived typing signal. */
      type: "typingStart";
      /** Typing signal payload. */
      data: {
        /** Room receiving the typing signal. */
        roomId: string;
        /** Typing user identifier. */
        userId: string;
        /** Typing user's login handle. */
        username: string;
      };
    }
  | {
      /** Event discriminator for a participant presence replacement. */
      type: "presenceUpdate";
      /** Presence replacement payload. */
      data: {
        /** Room whose participant view should change. */
        roomId: string;
        /** User whose presence changed. */
        userId: string;
        /** Sanitized server presence value. */
        status: string;
      };
    }
  | {
      /** Event discriminator for native upload progress. */
      type: "uploadProgress";
      /** Path-free upload progress payload. */
      data: {
        /** Room receiving the staged upload. */
        roomId: string;
        /** Native opaque identifier for this transfer. */
        transferId: string;
        /** Display filename without its local directory. */
        filename: string;
        /** Number of bytes accepted by the native transport. */
        bytesSent: number;
        /** Total validated size of the selected file. */
        totalBytes: number;
      };
    }
  | {
      /** Event discriminator for a native connection-state change. */
      type: "connectionChanged";
      /** Connection-state replacement payload. */
      data: {
        /** Open room affected by the transport change. */
        roomId: string;
        /** Current native transport state. */
        status: RoomConnectionStatus;
      };
    }
  | {
      /** Event discriminator for bounded reconnect reconciliation. */
      type: "reconciliation";
      /** Reconciliation payload. */
      data: {
        /** Open room receiving reconciled messages. */
        roomId: string;
        /** Oldest-first messages returned by the bounded algorithm. */
        page: MessagePage;
        /** Whether the bounded live window must be replaced instead of merged. */
        replaceLiveWindow: boolean;
      };
    };

/** One generation-scoped event ordered with native command results. */
export interface RoomConversationEventEnvelope {
  /** One-use token shared with the opening snapshot. */
  streamId: string;
  /** Strictly increasing sequence within this room generation. */
  sequence: number;
  /** Sanitized room update already committed to native state. */
  event: RoomConversationEvent;
}

/** One command value ordered on the same sequence as native room events. */
export interface RoomConversationCommandResult<T> {
  /** One-use token shared with the opening snapshot and events. */
  streamId: string;
  /** Strictly increasing sequence within this room generation. */
  sequence: number;
  /** Sanitized command value committed at this sequence. */
  value: T;
}

/** Event retained while React waits for the matching opening snapshot. */
export interface BufferedConversationEvent {
  /** Native generation and sequence envelope. */
  envelope: RoomConversationEventEnvelope;
  /** Local monotonic receipt time used for deterministic typing expiry. */
  receivedAt: number;
}

/** One active typing indicator with an explicit deterministic deadline. */
export interface ConversationTypingIndicator {
  /** Typing user identifier. */
  userId: string;
  /** Typing user's login handle. */
  username: string;
  /** Local time at which this indicator expires. */
  expiresAt: number;
}

/** Latest path-free progress for one native upload transfer. */
export interface ConversationUploadProgress {
  /** Native opaque transfer identifier. */
  transferId: string;
  /** Display filename without its local directory. */
  filename: string;
  /** Number of bytes accepted by the native transport. */
  bytesSent: number;
  /** Total validated size of the selected file. */
  totalBytes: number;
}

/** Retained partial edit that may arrive before its complete message. */
export interface PendingConversationMessageUpdate {
  /** Replacement message body. */
  content: string;
  /** ISO timestamp supplied by the edit event. */
  editedAt: string;
  /** Exact epoch-nanosecond value used for chronological comparison. */
  version: bigint;
}

/** Projection action waiting for a missing earlier shared sequence. */
export type ConversationTransition =
  | {
      /** Transition discriminator for one native event. */
      kind: "event";
      /** Sanitized event to project. */
      event: RoomConversationEvent;
      /** Local event receipt time. */
      receivedAt: number;
    }
  | {
      /** Transition discriminator for a message command result. */
      kind: "message";
      /** Complete returned message, or null when no message was committed. */
      message: RoomMessage | null;
    }
  | {
      /** Transition discriminator for a delete command result. */
      kind: "delete";
      /** Deleted message identifier. */
      messageId: string;
    }
  | {
      /** Transition discriminator for an older-page command result. */
      kind: "olderPage";
      /** Older bounded message page. */
      page: MessagePage;
    }
  | {
      /** Transition discriminator for a command without conversation projection data. */
      kind: "advance";
    };

/** Complete immutable React projection for one open room generation. */
export interface ConversationState {
  /** Open room identifier. */
  roomId: string;
  /** One-use token identifying the active native generation. */
  streamId: string;
  /** Highest contiguous shared sequence reflected in this projection. */
  lastEventSequence: number;
  /** Signed-in user identifier used for authorship affordances. */
  currentUserId: string;
  /** Current server-authoritative room capabilities. */
  permissions: RoomPermissions;
  /** Current bounded unread-divider placement. */
  unreadBoundary: RoomUnreadBoundary;
  /** Deterministically ordered visible message window. */
  messages: readonly RoomMessage[];
  /** Whether an explicit user action may request an older page. */
  hasOlder: boolean;
  /** Current native transport state. */
  connectionStatus: RoomConnectionStatus;
  /** Whether the user has explicitly remained at the newest visible edge. */
  atLiveEdge: boolean;
  /** Active typing indicators keyed by stable user identifier. */
  typingByUserId: ReadonlyMap<string, ConversationTypingIndicator>;
  /** Latest presence values keyed by stable user identifier. */
  presenceByUserId: ReadonlyMap<string, string>;
  /** Latest upload progress keyed by opaque transfer identifier. */
  uploadsByTransferId: ReadonlyMap<string, ConversationUploadProgress>;
  /** Latest known exact edit-or-create time keyed by message identifier. */
  messageVersions: ReadonlyMap<string, bigint>;
  /** Partial edits retained until their complete messages arrive. */
  pendingMessageUpdates: ReadonlyMap<string, PendingConversationMessageUpdate>;
  /** Generation-scoped deletion tombstones that prevent resurrection. */
  deletedMessageIds: ReadonlySet<string>;
  /** Out-of-order native actions waiting for a contiguous predecessor. */
  pendingTransitions: ReadonlyMap<number, ConversationTransition>;
}

/** Duration for which one typing event remains visible without a refresh. */
export const TYPING_EXPIRY_MS = 5_000;

/** Lowest comparison value reserved for a malformed native timestamp. */
const INVALID_TIMESTAMP_VERSION = -(1n << 127n);

/** Native RFC 3339 shape with an optional one-to-nine digit fractional second. */
const RFC3339_TIMESTAMP_PATTERN =
  /^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})(?:\.(\d{1,9}))?(Z|[+-]\d{2}:\d{2})$/;

/** Parse a native RFC 3339 timestamp into an exact epoch-nanosecond value. */
function timestampVersion(timestamp: string): bigint {
  const parts = RFC3339_TIMESTAMP_PATTERN.exec(timestamp);
  if (parts === null) {
    return INVALID_TIMESTAMP_VERSION;
  }
  const wholeSecondMilliseconds = Date.parse(`${parts[1]}${parts[3]}`);
  if (!Number.isFinite(wholeSecondMilliseconds)) {
    return INVALID_TIMESTAMP_VERSION;
  }
  const fractionalNanoseconds = BigInt(
    (parts[2] ?? "").padEnd(9, "0").slice(0, 9) || "0",
  );
  return BigInt(wholeSecondMilliseconds) * 1_000_000n + fractionalNanoseconds;
}

/** Return the authoritative edit-or-create version for one complete message. */
function messageVersion(message: RoomMessage): bigint {
  return timestampVersion(message.editedAt ?? message.createdAt);
}

/** Compare two messages by creation time and then opaque identifier. */
function compareMessages(left: RoomMessage, right: RoomMessage): number {
  const leftCreatedAt = timestampVersion(left.createdAt);
  const rightCreatedAt = timestampVersion(right.createdAt);
  if (leftCreatedAt !== rightCreatedAt) {
    return leftCreatedAt < rightCreatedAt ? -1 : 1;
  }
  if (left.id === right.id) {
    return 0;
  }
  return left.id < right.id ? -1 : 1;
}

/** Return a new deterministic oldest-first copy of one visible message window. */
function sortMessages(messages: readonly RoomMessage[]): RoomMessage[] {
  return [...messages].sort(compareMessages);
}

/** Insert or strictly advance one complete message without violating race ledgers. */
function upsertConversationMessage(
  state: ConversationState,
  incoming: RoomMessage,
): ConversationState {
  if (
    incoming.roomId !== state.roomId ||
    state.deletedMessageIds.has(incoming.id)
  ) {
    return state;
  }

  const incomingVersion = messageVersion(incoming);
  const pendingUpdate = state.pendingMessageUpdates.get(incoming.id);
  const candidate =
    pendingUpdate !== undefined && pendingUpdate.version >= incomingVersion
      ? {
          ...incoming,
          content: pendingUpdate.content,
          editedAt: pendingUpdate.editedAt,
        }
      : incoming;
  const candidateVersion = messageVersion(candidate);
  const knownVersion = state.messageVersions.get(candidate.id);
  if (knownVersion !== undefined && knownVersion > candidateVersion) {
    return state;
  }

  const existingIndex = state.messages.findIndex(
    (message) => message.id === candidate.id,
  );
  if (
    existingIndex >= 0 &&
    candidateVersion <= messageVersion(state.messages[existingIndex])
  ) {
    return state;
  }

  const messages = [...state.messages];
  if (existingIndex >= 0) {
    messages[existingIndex] = candidate;
  } else {
    messages.push(candidate);
  }
  const messageVersions = new Map(state.messageVersions);
  if (knownVersion === undefined || candidateVersion > knownVersion) {
    messageVersions.set(candidate.id, candidateVersion);
  }
  return {
    ...state,
    messages: sortMessages(messages),
    messageVersions,
  };
}

/** Apply one strictly newer partial edit and retain it for create-before-update races. */
function updateConversationMessage(
  state: ConversationState,
  messageId: string,
  content: string,
  editedAt: string,
): ConversationState {
  if (state.deletedMessageIds.has(messageId)) {
    return state;
  }
  const version = timestampVersion(editedAt);
  const knownVersion = state.messageVersions.get(messageId);
  if (knownVersion !== undefined && version <= knownVersion) {
    return state;
  }

  const messageVersions = new Map(state.messageVersions);
  messageVersions.set(messageId, version);
  const pendingMessageUpdates = new Map(state.pendingMessageUpdates);
  pendingMessageUpdates.set(messageId, { content, editedAt, version });
  const messages = state.messages.map((message) =>
    message.id === messageId ? { ...message, content, editedAt } : message,
  );
  return {
    ...state,
    messages,
    messageVersions,
    pendingMessageUpdates,
  };
}

/** Move a divider whose target was deleted to the next known unread position. */
function boundaryAfterDeletion(
  state: ConversationState,
  messageId: string,
  deletedIndex: number,
  remainingMessages: readonly RoomMessage[],
): RoomUnreadBoundary {
  if (
    state.unreadBoundary.kind !== "beforeMessage" ||
    state.unreadBoundary.messageId !== messageId
  ) {
    return state.unreadBoundary;
  }
  if (deletedIndex >= 0 && deletedIndex < remainingMessages.length) {
    return {
      kind: "beforeMessage",
      messageId: remainingMessages[deletedIndex].id,
    };
  }
  return state.hasOlder && deletedIndex < 0
    ? { kind: "beforeLoadedWindow" }
    : { kind: "none" };
}

/** Tombstone and remove one message while preserving generation idempotency. */
function deleteConversationMessage(
  state: ConversationState,
  messageId: string,
): ConversationState {
  if (state.deletedMessageIds.has(messageId)) {
    return state;
  }
  const deletedIndex = state.messages.findIndex(
    (message) => message.id === messageId,
  );
  const messages = state.messages.filter((message) => message.id !== messageId);
  const deletedMessageIds = new Set(state.deletedMessageIds);
  deletedMessageIds.add(messageId);
  const messageVersions = new Map(state.messageVersions);
  messageVersions.delete(messageId);
  const pendingMessageUpdates = new Map(state.pendingMessageUpdates);
  pendingMessageUpdates.delete(messageId);
  return {
    ...state,
    messages,
    unreadBoundary: boundaryAfterDeletion(
      state,
      messageId,
      deletedIndex,
      messages,
    ),
    messageVersions,
    pendingMessageUpdates,
    deletedMessageIds,
  };
}

/** Reconcile one bounded page by either merging or replacing the visible window. */
function reconcileConversationPage(
  state: ConversationState,
  page: MessagePage,
  replaceLiveWindow: boolean,
  replaceHasOlder: boolean,
): ConversationState {
  const pageIds = new Set(page.messages.map((message) => message.id));
  let nextState = replaceLiveWindow
    ? {
        ...state,
        messages: state.messages.filter((message) => pageIds.has(message.id)),
      }
    : state;
  for (const message of page.messages) {
    nextState = upsertConversationMessage(nextState, message);
  }
  const hasOlder = replaceHasOlder ? page.hasOlder : state.hasOlder;
  let unreadBoundary = nextState.unreadBoundary;
  if (replaceLiveWindow && unreadBoundary.kind === "beforeMessage") {
    const unreadMessageId = unreadBoundary.messageId;
    if (!nextState.messages.some((message) => message.id === unreadMessageId)) {
      unreadBoundary = hasOlder
        ? { kind: "beforeLoadedWindow" }
        : { kind: "none" };
    }
  }
  if (
    nextState.hasOlder === hasOlder &&
    nextState.unreadBoundary === unreadBoundary
  ) {
    return nextState;
  }
  return {
    ...nextState,
    hasOlder,
    unreadBoundary,
  };
}

/** Establish the oldest newly visible other-user message as the first unread item. */
function establishConversationUnreadBoundary(
  previousState: ConversationState,
  projectedState: ConversationState,
  candidates: readonly RoomMessage[],
): ConversationState {
  if (projectedState.unreadBoundary.kind !== "none") {
    return projectedState;
  }
  const previousIds = new Set(
    previousState.messages.map((message) => message.id),
  );
  const candidateIds = new Set(candidates.map((message) => message.id));
  const firstUnread = projectedState.messages.find(
    (message) =>
      candidateIds.has(message.id) &&
      !previousIds.has(message.id) &&
      message.authorId !== projectedState.currentUserId,
  );
  return firstUnread === undefined
    ? projectedState
    : {
        ...projectedState,
        unreadBoundary: {
          kind: "beforeMessage",
          messageId: firstUnread.id,
        },
      };
}

/** Apply one already-contiguous event to the visible projection. */
function projectConversationEvent(
  state: ConversationState,
  event: RoomConversationEvent,
  receivedAt: number,
): ConversationState {
  switch (event.type) {
    case "messageCreate": {
      const projected = upsertConversationMessage(state, event.data.message);
      return establishConversationUnreadBoundary(state, projected, [
        event.data.message,
      ]);
    }
    case "messageUpdate":
      return updateConversationMessage(
        state,
        event.data.messageId,
        event.data.content,
        event.data.editedAt,
      );
    case "messageDelete":
      return deleteConversationMessage(state, event.data.messageId);
    case "typingStart": {
      const typingByUserId = new Map(state.typingByUserId);
      typingByUserId.set(event.data.userId, {
        userId: event.data.userId,
        username: event.data.username,
        expiresAt: receivedAt + TYPING_EXPIRY_MS,
      });
      return { ...state, typingByUserId };
    }
    case "presenceUpdate": {
      const presenceByUserId = new Map(state.presenceByUserId);
      presenceByUserId.set(event.data.userId, event.data.status);
      return { ...state, presenceByUserId };
    }
    case "uploadProgress": {
      const uploadsByTransferId = new Map(state.uploadsByTransferId);
      uploadsByTransferId.set(event.data.transferId, {
        transferId: event.data.transferId,
        filename: event.data.filename,
        bytesSent: event.data.bytesSent,
        totalBytes: event.data.totalBytes,
      });
      return { ...state, uploadsByTransferId };
    }
    case "connectionChanged":
      return state.connectionStatus === event.data.status
        ? state
        : { ...state, connectionStatus: event.data.status };
    case "reconciliation": {
      const projected = reconcileConversationPage(
        state,
        event.data.page,
        event.data.replaceLiveWindow,
        event.data.replaceLiveWindow,
      );
      return establishConversationUnreadBoundary(
        state,
        projected,
        event.data.page.messages,
      );
    }
  }
}

/** Apply one already-contiguous event or command transition. */
function projectConversationTransition(
  state: ConversationState,
  transition: ConversationTransition,
): ConversationState {
  switch (transition.kind) {
    case "event":
      return projectConversationEvent(
        state,
        transition.event,
        transition.receivedAt,
      );
    case "message":
      return transition.message === null
        ? state
        : upsertConversationMessage(state, transition.message);
    case "delete":
      return deleteConversationMessage(state, transition.messageId);
    case "olderPage":
      return reconcileConversationPage(state, transition.page, false, true);
    case "advance":
      return state;
  }
}

/** Drain every transition made contiguous by the latest insertion. */
function drainConversationTransitions(
  state: ConversationState,
): ConversationState {
  let nextState = state;
  while (Number.isSafeInteger(nextState.lastEventSequence + 1)) {
    const sequence = nextState.lastEventSequence + 1;
    const transition = nextState.pendingTransitions.get(sequence);
    if (transition === undefined) {
      break;
    }
    const pendingTransitions = new Map(nextState.pendingTransitions);
    pendingTransitions.delete(sequence);
    const projected = projectConversationTransition(
      { ...nextState, pendingTransitions },
      transition,
    );
    nextState = { ...projected, lastEventSequence: sequence };
  }
  return nextState;
}

/** Insert one generation-matching transition and project only contiguous sequences. */
function enqueueConversationTransition(
  state: ConversationState,
  streamId: string,
  sequence: number,
  transition: ConversationTransition,
): ConversationState {
  if (
    streamId !== state.streamId ||
    !Number.isSafeInteger(sequence) ||
    sequence <= state.lastEventSequence ||
    state.pendingTransitions.has(sequence)
  ) {
    return state;
  }
  const pendingTransitions = new Map(state.pendingTransitions);
  pendingTransitions.set(sequence, transition);
  return drainConversationTransitions({ ...state, pendingTransitions });
}

/** Replace all generation-scoped state and replay matching post-snapshot events. */
export function replaceConversationSnapshot(
  _previous: ConversationState | null,
  snapshot: RoomConversationSnapshot,
  bufferedEvents: readonly BufferedConversationEvent[] = [],
): ConversationState {
  let state: ConversationState = {
    roomId: snapshot.roomId,
    streamId: snapshot.streamId,
    lastEventSequence: snapshot.lastEventSequence,
    currentUserId: snapshot.currentUserId,
    permissions: snapshot.permissions,
    unreadBoundary: snapshot.unreadBoundary,
    messages: [],
    hasOlder: snapshot.page.hasOlder,
    connectionStatus: snapshot.connectionStatus,
    atLiveEdge: true,
    typingByUserId: new Map(),
    presenceByUserId: new Map(),
    uploadsByTransferId: new Map(),
    messageVersions: new Map(),
    pendingMessageUpdates: new Map(),
    deletedMessageIds: new Set(),
    pendingTransitions: new Map(),
  };
  for (const message of snapshot.page.messages) {
    state = upsertConversationMessage(state, message);
  }
  for (const buffered of bufferedEvents) {
    state = applyConversationEvent(
      state,
      buffered.envelope,
      buffered.receivedAt,
    );
  }
  return state;
}

/** Order one native event without advancing past gaps or foreign generations. */
export function applyConversationEvent(
  state: ConversationState,
  envelope: RoomConversationEventEnvelope,
  receivedAt: number,
): ConversationState {
  if (envelope.event.data.roomId !== state.roomId) {
    return state;
  }
  return enqueueConversationTransition(state, envelope.streamId, envelope.sequence, {
    kind: "event",
    event: envelope.event,
    receivedAt,
  });
}

/** Order one send-or-edit command result on the shared conversation sequence. */
export function applyConversationMessageResult(
  state: ConversationState,
  result: RoomConversationCommandResult<RoomMessage | null>,
): ConversationState {
  return enqueueConversationTransition(state, result.streamId, result.sequence, {
    kind: "message",
    message: result.value,
  });
}

/** Order one delete command result on the shared conversation sequence. */
export function applyConversationDeleteResult(
  state: ConversationState,
  result: RoomConversationCommandResult<string>,
): ConversationState {
  return enqueueConversationTransition(state, result.streamId, result.sequence, {
    kind: "delete",
    messageId: result.value,
  });
}

/** Order and prepend one older bounded page without disturbing newer messages. */
export function applyConversationOlderPageResult(
  state: ConversationState,
  result: RoomConversationCommandResult<MessagePage>,
): ConversationState {
  return enqueueConversationTransition(state, result.streamId, result.sequence, {
    kind: "olderPage",
    page: result.value,
  });
}

/** Consume one shared command sequence whose value does not alter this projection. */
export function advanceConversationCommand<T>(
  state: ConversationState,
  result: RoomConversationCommandResult<T>,
): ConversationState {
  return enqueueConversationTransition(state, result.streamId, result.sequence, {
    kind: "advance",
  });
}

/** Move the unread divider immediately beyond one loaded message. */
export function markConversationReadThrough(
  state: ConversationState,
  messageId: string,
): ConversationState {
  const index = state.messages.findIndex((message) => message.id === messageId);
  if (index < 0 || state.unreadBoundary.kind === "none") {
    return state;
  }
  if (state.unreadBoundary.kind === "beforeMessage") {
    const currentUnreadMessageId = state.unreadBoundary.messageId;
    const currentUnreadIndex = state.messages.findIndex(
      (message) => message.id === currentUnreadMessageId,
    );
    if (currentUnreadIndex < 0 || index < currentUnreadIndex) {
      return state;
    }
  }
  const nextMessage = state.messages[index + 1];
  const unreadBoundary: RoomUnreadBoundary =
    nextMessage === undefined
      ? { kind: "none" }
      : { kind: "beforeMessage", messageId: nextMessage.id };
  if (
    state.unreadBoundary.kind === unreadBoundary.kind &&
    (unreadBoundary.kind !== "beforeMessage" ||
      (state.unreadBoundary.kind === "beforeMessage" &&
        state.unreadBoundary.messageId === unreadBoundary.messageId))
  ) {
    return state;
  }
  return { ...state, unreadBoundary };
}

/** Replace the explicit newest-edge state without coupling it to message arrival. */
export function setConversationLiveEdge(
  state: ConversationState,
  atLiveEdge: boolean,
): ConversationState {
  return state.atLiveEdge === atLiveEdge ? state : { ...state, atLiveEdge };
}

/** Remove typing indicators whose deterministic local deadlines have elapsed. */
export function expireConversationTyping(
  state: ConversationState,
  now: number,
): ConversationState {
  const expiredUserIds = [...state.typingByUserId.entries()]
    .filter(([, indicator]) => indicator.expiresAt <= now)
    .map(([userId]) => userId);
  if (expiredUserIds.length === 0) {
    return state;
  }
  const typingByUserId = new Map(state.typingByUserId);
  for (const userId of expiredUserIds) {
    typingByUserId.delete(userId);
  }
  return { ...state, typingByUserId };
}
