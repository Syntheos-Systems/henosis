/** Explicit fixture adapter used by browser development and component tests. */
import type { UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentCapabilityCatalog,
  AgentCredentialReadiness,
  AgentIdentity,
  AgentRosterSnapshot,
  AgentSeatDraft,
  ApplyAgentRosterRequest,
  OwnedAgentIdentity,
  RoomBridgeStatus,
} from "../domain/agentControl";
import type {
  MessagePage,
  PendingRoomAttachment,
  RoomConversationCommandResult,
  RoomConversationEvent,
  RoomConversationSnapshot,
  RoomMessage,
  RoomPermissions,
  RoomUnreadBoundary,
} from "../domain/conversation";
import type { RoomSummary } from "../domain/rooms";
import {
  createFixtureAgentCatalog,
  createFixtureAgentIdentities,
  createFixtureAgentRosters,
  createFixtureRooms,
} from "../data/fixtureRooms";
import { HenosisClientError } from "./henosisClient";
import type {
  BootstrapResult,
  HenosisClient,
  RiftConnectionInput,
  RoomDirectorySnapshot,
  RoomEventListener,
  SanitizedConnection,
} from "./henosisClient";

/** Number of newest fixture messages exposed by the opening snapshot. */
const FIXTURE_PAGE_SIZE = 3;

/** Native attachment count limit mirrored by the browser fixture. */
const MAX_FIXTURE_ATTACHMENTS = 10;

/** Native per-session one-use stream capability limit. */
const MAX_FIXTURE_STREAM_IDS = 65_536;

/** Native URL-safe stream capability shape mirrored by the fixture. */
const VALID_FIXTURE_STREAM_ID = /^[A-Za-z0-9_-]{16,128}$/;

/** Stable path-free fixture attachment selected by the browser adapter. */
const FIXTURE_ATTACHMENT = {
  filename: "fixture-note.txt",
  contentType: "text/plain",
  sizeBytes: 128,
} as const;

/** Default sanitized identity used before the preview connection form runs. */
const DEFAULT_FIXTURE_CONNECTION: SanitizedConnection = {
  endpoint: "http://127.0.0.1:4010",
  username: "operator",
  userId: "fixture-user",
  displayName: "Operator",
};

/** Mutable state owned by one exact fixture room generation. */
interface OpenFixtureRoom {
  /** Room bound to this generation. */
  roomId: string;
  /** One-use generation identifier supplied by React. */
  streamId: string;
  /** Highest shared command or event sequence already issued. */
  sequence: number;
  /** Messages currently eligible for edit, delete, and read commands. */
  loadedMessageIds: Set<string>;
}

/** One deterministic failure consumed by the next pending activation poll. */
interface FixtureActivationFailure {
  /** Stable safe runtime failure code. */
  code: string;
  /** Safe path-free fixture recovery detail. */
  message: string;
}

/** Clone one primitive settings object used by a fixture seat. */
function cloneAgentSettings(
  settings: Readonly<Record<string, unknown>>,
): Record<string, unknown> {
  return Object.fromEntries(Object.entries(settings));
}

/** Test whether one fixture identity is owned by the selected human. */
function identityIsOwnedBy(
  identity: AgentIdentity,
  ownerUserId: string,
): identity is OwnedAgentIdentity {
  return identity.ownerUserId === ownerUserId;
}

/** Clone one deployment capability catalog through every nested collection. */
function cloneAgentCatalog(
  catalog: AgentCapabilityCatalog,
): AgentCapabilityCatalog {
  return {
    generation: catalog.generation,
    harnesses: catalog.harnesses.map((harness) => ({
      ...harness,
      models: harness.models.map((model) => ({ ...model })),
      settings: harness.settings.map((setting) => ({
        ...setting,
        control:
          setting.control.type === "select"
            ? {
                type: "select",
                options: setting.control.options.map((option) => ({ ...option })),
              }
            : { ...setting.control },
      })),
    })),
  };
}

/** Clone one authoritative fixture roster and all non-secret settings objects. */
function cloneAgentRoster(roster: AgentRosterSnapshot): AgentRosterSnapshot {
  return {
    ...roster,
    seats: roster.seats.map((seat) => ({
      ...seat,
      settings: cloneAgentSettings(seat.settings),
    })),
  };
}

/** Build one deterministic sanitized fixture message. */
function createFixtureMessage(roomId: string, ordinal: number): RoomMessage {
  const createdAt = new Date(Date.UTC(2026, 7, 1, 12, ordinal)).toISOString();
  const authoredByAgent = ordinal % 2 === 1;

  return {
    id: `${roomId}-message-${ordinal}`,
    roomId,
    authorId: authoredByAgent ? "fixture-agent" : "fixture-user",
    authorUsername: authoredByAgent ? "mira" : "operator",
    authorDisplayName: authoredByAgent ? "Mira" : "Operator",
    authorAvatarUrl: null,
    content: `Fixture conversation message ${ordinal}.`,
    editedAt: null,
    createdAt,
    messageType: "user",
    attachments: [],
  };
}

/** Copy one message so fixture callers cannot mutate retained adapter state. */
function cloneMessage(message: RoomMessage): RoomMessage {
  return {
    ...message,
    attachments: message.attachments.map((attachment) => ({ ...attachment })),
  };
}

/** Browser-only client that never claims fixture rooms came from a live Rift. */
export class FixtureHenosisClient implements HenosisClient {
  /** Sanitized preview identity retained without the submitted password. */
  private connection: SanitizedConnection = { ...DEFAULT_FIXTURE_CONNECTION };

  /** Whether the browser fixture currently owns an authenticated preview session. */
  private connected = true;

  /** Persistent oldest-first fixture histories keyed by room identifier. */
  private readonly histories = new Map<string, RoomMessage[]>();

  /** Monotonic per-room read markers retained across replacement generations. */
  private readonly readMarkers = new Map<string, string>();

  /** Path-free uploads waiting to be attached to a fixture message. */
  private readonly pendingAttachments = new Map<string, PendingRoomAttachment>();

  /** Event subscribers sharing the same boundary as the Tauri adapter. */
  private readonly roomEventListeners = new Set<RoomEventListener>();

  /** One-use room stream capabilities retained for the current fixture session. */
  private readonly usedRoomStreamIds = new Set<string>();

  /** Sole active room generation, replaced atomically by every open. */
  private openGeneration: OpenFixtureRoom | null = null;

  /** Counter used to produce unique fixture-created message identifiers. */
  private createdMessageCounter = 0;

  /** Counter used to produce opaque fixture upload identifiers. */
  private uploadCounter = 0;

  /** Deterministic minute offset used for fixture message and edit timestamps. */
  private logicalMinute = 6;

  /** Persistent fixture identities cloned per client to prevent cross-test state. */
  private readonly agentIdentities = createFixtureAgentIdentities();

  /** Deployment-style fixture catalog cloned before every public return. */
  private readonly agentCatalog = createFixtureAgentCatalog();

  /** Authoritative fixture rosters keyed by their room server identifier. */
  private readonly agentRosters = new Map(
    createFixtureAgentRosters().map((roster) => [roster.serverId, roster]),
  );

  /** Independent public pause state retained for each fixture bridge. */
  private readonly pausedAgentBridges = new Map<string, boolean>([
    ["server-operations", true],
  ]);

  /** One-shot activation failures injected explicitly by deterministic tests. */
  private readonly nextAgentActivationFailures = new Map<
    string,
    FixtureActivationFailure
  >();

  /** Counter used to create stable fixture-owned agent identifiers. */
  private createdAgentCounter = 0;

  /** Return a fixture-backed connected directory for visual development. */
  async bootstrap(): Promise<BootstrapResult> {
    return {
      directory: this.snapshot(),
      requiresAuthentication: false,
    };
  }

  /** Accept GUI credentials without retaining the password and return fixture data. */
  async connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot> {
    this.openGeneration = null;
    this.pendingAttachments.clear();
    this.usedRoomStreamIds.clear();
    this.connected = true;
    this.connection = {
      endpoint: input.endpoint,
      username: input.username,
      userId: "fixture-user",
      displayName: input.username || "Preview operator",
    };
    return this.snapshot();
  }

  /** Refresh fixture timestamps and room data. */
  async refresh(): Promise<RoomDirectorySnapshot> {
    return this.snapshot();
  }

  /** End the fixture session and invalidate its active conversation generation. */
  async disconnect(): Promise<void> {
    this.openGeneration = null;
    this.pendingAttachments.clear();
    this.usedRoomStreamIds.clear();
    this.connected = false;
  }

  /** List persistent identities owned by the current fixture human. */
  async getMyAgents(): Promise<OwnedAgentIdentity[]> {
    this.requireConnectedSession();
    return this.agentIdentities
      .filter((identity) => identityIsOwnedBy(identity, this.connection.userId))
      .map((identity) => ({ ...identity }));
  }

  /** Create one new fixture identity owned by the current fixture human. */
  async createMyAgent(
    username: string,
    displayName: string | null,
  ): Promise<OwnedAgentIdentity> {
    this.requireConnectedSession();
    if (!username || username.trim() !== username) {
      throw new HenosisClientError(
        "validation",
        "Choose a non-empty fixture agent username without surrounding spaces.",
        "agent_username_invalid",
      );
    }
    if (
      this.agentIdentities.some(
        (identity) =>
          identity.username.toLowerCase() === username.toLowerCase(),
      )
    ) {
      throw new HenosisClientError(
        "conflict",
        "That fixture agent username is already in use.",
        "agent_username_taken",
      );
    }
    this.createdAgentCounter += 1;
    const identity: OwnedAgentIdentity = {
      id: `fixture-agent-created-${this.createdAgentCounter}`,
      username,
      displayName: displayName?.trim() || null,
      ownerUserId: this.connection.userId,
    };
    this.agentIdentities.push(identity);
    return { ...identity };
  }

  /** Claim one known imported fixture identity without changing room membership. */
  async claimAgent(agentIdentityId: string): Promise<OwnedAgentIdentity> {
    this.requireConnectedSession();
    const identityIndex = this.agentIdentities.findIndex(
      (identity) => identity.id === agentIdentityId,
    );
    const identity = this.agentIdentities[identityIndex];
    if (!identity) {
      throw new HenosisClientError(
        "validation",
        "That imported fixture identity is not known to this room context.",
        "agent_identity_not_found",
      );
    }
    if (identity.ownerUserId === this.connection.userId) {
      throw new HenosisClientError(
        "conflict",
        "That fixture identity is already owned by the current human.",
        "agent_already_owned",
      );
    }
    if (identity.ownerUserId !== null) {
      throw new HenosisClientError(
        "forbidden",
        "That fixture identity is owned by another human.",
        "agent_owned_by_another_human",
      );
    }
    const claimed: OwnedAgentIdentity = {
      ...identity,
      ownerUserId: this.connection.userId,
    };
    this.agentIdentities.splice(identityIndex, 1);
    this.agentIdentities.push(claimed);
    for (const [serverId, roster] of this.agentRosters) {
      const seats = roster.seats.map((seat) =>
        seat.agentIdentityId === claimed.id
          ? { ...seat, ownerHumanId: claimed.ownerUserId }
          : seat,
      );
      this.agentRosters.set(serverId, cloneAgentRoster({ ...roster, seats }));
    }
    return { ...claimed };
  }

  /** Load the deployment-style fixture capability catalog for one known server. */
  async getAgentCapabilities(serverId: string): Promise<AgentCapabilityCatalog> {
    this.requireAgentRoster(serverId);
    return cloneAgentCatalog(this.agentCatalog);
  }

  /** Load deterministic authoritative permissions for one known fixture server. */
  async getRoomPermissions(serverId: string): Promise<RoomPermissions> {
    return this.fixtureRoomPermissions(serverId);
  }

  /** Load one cloned authoritative desired fixture roster. */
  async getRoomAgentRoster(serverId: string): Promise<AgentRosterSnapshot> {
    return cloneAgentRoster(this.requireAgentRoster(serverId));
  }

  /** Apply one optimistic complete fixture roster replacement atomically. */
  async applyRoomAgentRoster(
    serverId: string,
    update: ApplyAgentRosterRequest,
  ): Promise<AgentRosterSnapshot> {
    const current = this.requireAgentRoster(serverId);
    if (update.expectedRevision !== current.desiredRevision) {
      throw new HenosisClientError(
        "conflict",
        "The fixture roster changed. Reload it before applying again.",
        "revision_conflict",
      );
    }
    const nextRevision = (current.desiredRevision ?? 0) + 1;
    const seats = update.seats.map((seat) => {
      const identity = this.agentIdentities.find(
        (candidate) => candidate.id === seat.agentIdentityId,
      );
      if (!identity) {
        throw new HenosisClientError(
          "validation",
          "That fixture agent identity is not available.",
          "fixture_agent_not_found",
        );
      }
      return {
        ...seat,
        agentUsername: identity.username,
        agentDisplayName: identity.displayName,
        settings: cloneAgentSettings(seat.settings),
        ownerHumanId: identity.ownerUserId,
        configurationRevision: nextRevision,
        credentialReadiness: this.fixtureCredentialReadiness(seat),
        runtimeActivation: "pending" as const,
      };
    });
    const applied: AgentRosterSnapshot = {
      serverId,
      desiredRevision: nextRevision,
      activeRevision: current.activeRevision,
      lastGoodRevision: current.lastGoodRevision,
      runtimeActivation: "pending",
      runtimeErrorCode: null,
      runtimeErrorMessage: null,
      seats,
    };
    this.agentRosters.set(serverId, cloneAgentRoster(applied));
    return cloneAgentRoster(applied);
  }

  /** Poll one bridge and deterministically settle its current pending revision. */
  async getRoomBridgeStatus(serverId: string): Promise<RoomBridgeStatus> {
    this.settlePendingAgentActivation(serverId);
    return this.fixtureBridgeStatus(serverId);
  }

  /** Pause one known fixture bridge without changing roster revisions. */
  async pauseRoomBridge(serverId: string): Promise<RoomBridgeStatus> {
    this.requireAgentRoster(serverId);
    this.pausedAgentBridges.set(serverId, true);
    return this.fixtureBridgeStatus(serverId);
  }

  /** Resume one known fixture bridge without changing roster revisions. */
  async resumeRoomBridge(serverId: string): Promise<RoomBridgeStatus> {
    this.requireAgentRoster(serverId);
    this.pausedAgentBridges.set(serverId, false);
    return this.fixtureBridgeStatus(serverId);
  }

  /** Retry the current desired fixture revision without creating another revision. */
  async reconcileRoomBridge(serverId: string): Promise<AgentRosterSnapshot> {
    const current = this.requireAgentRoster(serverId);
    if (current.desiredRevision === null) {
      return cloneAgentRoster(current);
    }
    const retrying: AgentRosterSnapshot = {
      ...current,
      runtimeActivation: "pending",
      runtimeErrorCode: null,
      runtimeErrorMessage: null,
      seats: current.seats.map((seat) => ({
        ...seat,
        settings: cloneAgentSettings(seat.settings),
        runtimeActivation: "pending",
      })),
    };
    this.agentRosters.set(serverId, cloneAgentRoster(retrying));
    return cloneAgentRoster(retrying);
  }

  /** Inject one safe deterministic failure for the next pending activation poll. */
  injectNextAgentActivationFailure(serverId: string, code: string): void {
    this.requireAgentRoster(serverId);
    if (!/^[a-z0-9_]{1,64}$/.test(code)) {
      throw new HenosisClientError(
        "validation",
        "Choose a lowercase fixture activation code.",
        "fixture_failure_code_invalid",
      );
    }
    this.nextAgentActivationFailures.set(serverId, {
      code,
      message: "The fixture bridge could not activate the desired roster.",
    });
  }

  /** Open one replacement generation over the newest bounded message window. */
  async openRoom(
    roomId: string,
    streamId: string,
  ): Promise<RoomConversationSnapshot> {
    this.requireConnectedSession();
    const room = this.requireKnownRoom(roomId);
    this.reserveRoomStreamId(streamId);
    const history = this.roomHistory(roomId);
    const messages = history.slice(-FIXTURE_PAGE_SIZE).map(cloneMessage);
    this.pendingAttachments.clear();
    const openGeneration: OpenFixtureRoom = {
      roomId,
      streamId,
      sequence: 0,
      loadedMessageIds: new Set(messages.map((message) => message.id)),
    };
    this.openGeneration = openGeneration;

    return {
      roomId,
      streamId,
      lastEventSequence: 0,
      currentUserId: this.connection.userId,
      permissions: this.fixtureRoomPermissions(room.serverId),
      unreadBoundary: this.unreadBoundary(roomId, history, messages),
      page: {
        messages,
        hasOlder: history.length > messages.length,
      },
      connectionStatus: "connected",
    };
  }

  /** Close only the currently active exact room generation. */
  async closeRoom(roomId: string, streamId: string): Promise<void> {
    this.requireOpenGeneration(roomId, streamId);
    this.openGeneration = null;
    this.pendingAttachments.clear();
  }

  /** Return the bounded history immediately before the oldest loaded message. */
  async loadOlderMessages(
    roomId: string,
    streamId: string,
    beforeMessageId: string,
  ): Promise<RoomConversationCommandResult<MessagePage>> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    const history = this.roomHistory(roomId);
    const loadedIndices = [...openGeneration.loadedMessageIds]
      .map((messageId) => history.findIndex((message) => message.id === messageId))
      .filter((index) => index >= 0);
    const oldestLoadedIndex = Math.min(...loadedIndices);

    if (history[oldestLoadedIndex]?.id !== beforeMessageId) {
      throw new HenosisClientError(
        "validation",
        "Older fixture messages must be requested from the current oldest cursor.",
      );
    }

    const pageStart = Math.max(0, oldestLoadedIndex - FIXTURE_PAGE_SIZE);
    const messages = history.slice(pageStart, oldestLoadedIndex).map(cloneMessage);
    for (const message of messages) {
      openGeneration.loadedMessageIds.add(message.id);
    }

    return this.commandResult(openGeneration, {
      messages,
      hasOlder: pageStart > 0,
    });
  }

  /** Commit one fixture message and emit its ordered duplicate create event. */
  async sendRoomMessage(
    roomId: string,
    streamId: string,
    content: string,
    pendingUploadIds: string[],
  ): Promise<RoomConversationCommandResult<RoomMessage | null>> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    this.validatePendingUploadIds(pendingUploadIds);
    const attachments = pendingUploadIds.map((uploadId) => {
      const pending = this.pendingAttachments.get(uploadId);
      if (!pending) {
        throw new HenosisClientError(
          "validation",
          "That staged fixture attachment is no longer available.",
        );
      }
      return pending;
    });

    if (content.trim().length === 0 && attachments.length === 0) {
      throw new HenosisClientError(
        "validation",
        "A fixture message needs text or at least one attachment.",
      );
    }

    this.createdMessageCounter += 1;
    const messageId = `${roomId}-created-${this.createdMessageCounter}`;
    const message: RoomMessage = {
      id: messageId,
      roomId,
      authorId: this.connection.userId,
      authorUsername: this.connection.username,
      authorDisplayName: this.connection.displayName,
      authorAvatarUrl: null,
      content,
      editedAt: null,
      createdAt: this.nextTimestamp(),
      messageType: "user",
      attachments: attachments.map((pending, index) => ({
        id: `${messageId}-attachment-${index + 1}`,
        filename: pending.filename,
        url: this.attachmentUrl(pending.uploadId),
        contentType: pending.contentType,
        sizeBytes: pending.sizeBytes,
      })),
    };

    this.roomHistory(roomId).push(message);
    openGeneration.loadedMessageIds.add(message.id);
    for (const pending of attachments) {
      this.pendingAttachments.delete(pending.uploadId);
    }

    const result = this.commandResult(openGeneration, cloneMessage(message));
    this.emit(openGeneration, {
      type: "messageCreate",
      data: { roomId, message: cloneMessage(message) },
    });
    return result;
  }

  /** Replace one loaded fixture message and emit its ordered update event. */
  async editRoomMessage(
    roomId: string,
    streamId: string,
    messageId: string,
    content: string,
  ): Promise<RoomConversationCommandResult<RoomMessage | null>> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    if (content.trim().length === 0) {
      throw new HenosisClientError(
        "validation",
        "Edited fixture messages cannot be empty.",
      );
    }
    const message = this.requireLoadedMessage(openGeneration, messageId);
    const editedAt = this.nextTimestamp();
    message.content = content;
    message.editedAt = editedAt;

    const result = this.commandResult(openGeneration, cloneMessage(message));
    this.emit(openGeneration, {
      type: "messageUpdate",
      data: { roomId, messageId, content, editedAt },
    });
    return result;
  }

  /** Remove one loaded fixture message and emit its ordered delete event. */
  async deleteRoomMessage(
    roomId: string,
    streamId: string,
    messageId: string,
  ): Promise<RoomConversationCommandResult<string>> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    this.requireLoadedMessage(openGeneration, messageId);
    const history = this.roomHistory(roomId);
    const messageIndex = history.findIndex((message) => message.id === messageId);
    history.splice(messageIndex, 1);
    openGeneration.loadedMessageIds.delete(messageId);

    const result = this.commandResult(openGeneration, messageId);
    this.emit(openGeneration, {
      type: "messageDelete",
      data: { roomId, messageId },
    });
    return result;
  }

  /** Stage one deterministic path-free upload and emit bounded progress events. */
  async selectAndUploadRoomAttachments(
    roomId: string,
    streamId: string,
  ): Promise<RoomConversationCommandResult<PendingRoomAttachment[]>> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    this.uploadCounter += 1;
    const pending: PendingRoomAttachment = {
      uploadId: `fixture-upload-${this.uploadCounter}`,
      ...FIXTURE_ATTACHMENT,
    };
    this.pendingAttachments.set(pending.uploadId, pending);

    this.emit(openGeneration, {
      type: "uploadProgress",
      data: {
        roomId,
        transferId: pending.uploadId,
        filename: pending.filename,
        bytesSent: 0,
        totalBytes: pending.sizeBytes,
      },
    });
    this.emit(openGeneration, {
      type: "uploadProgress",
      data: {
        roomId,
        transferId: pending.uploadId,
        filename: pending.filename,
        bytesSent: pending.sizeBytes,
        totalBytes: pending.sizeBytes,
      },
    });

    return this.commandResult(openGeneration, [{ ...pending }]);
  }

  /** Emit one fixture typing signal for the exact open generation. */
  async sendRoomTyping(roomId: string, streamId: string): Promise<void> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    this.emit(openGeneration, {
      type: "typingStart",
      data: {
        roomId,
        userId: this.connection.userId,
        username: this.connection.username,
      },
    });
  }

  /** Persist a monotonic read marker for one currently loaded message. */
  async markRoomRead(
    roomId: string,
    streamId: string,
    messageId: string,
  ): Promise<void> {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    this.requireLoadedMessage(openGeneration, messageId);
    const history = this.roomHistory(roomId);
    const currentMarkerId = this.readMarkers.get(roomId);
    const currentIndex = currentMarkerId
      ? history.findIndex((message) => message.id === currentMarkerId)
      : -1;
    const nextIndex = history.findIndex((message) => message.id === messageId);
    if (nextIndex > currentIndex) {
      this.readMarkers.set(roomId, messageId);
    }
  }

  /** Subscribe to fixture events and return an idempotent cleanup function. */
  async subscribeRoomEvents(listener: RoomEventListener): Promise<UnlistenFn> {
    this.roomEventListeners.add(listener);
    let listening = true;
    return () => {
      if (listening) {
        listening = false;
        this.roomEventListeners.delete(listener);
      }
    };
  }

  /** Publish one validated native-style event through the active fixture stream. */
  emitRoomEvent(
    roomId: string,
    streamId: string,
    event: RoomConversationEvent,
  ): void {
    const openGeneration = this.requireOpenGeneration(roomId, streamId);
    if (event.data.roomId !== roomId) {
      throw new HenosisClientError(
        "validation",
        "A fixture room event must target its active room.",
      );
    }
    this.emit(openGeneration, event);
  }

  /** Return one known fixture roster only while the preview session is connected. */
  private requireAgentRoster(serverId: string): AgentRosterSnapshot {
    this.requireConnectedSession();
    const roster = this.agentRosters.get(serverId);
    if (!roster) {
      throw new HenosisClientError(
        "validation",
        "That server is not available in the browser fixture.",
        "fixture_server_not_found",
      );
    }
    return roster;
  }

  /** Return a fresh permission snapshot for one known fixture server. */
  private fixtureRoomPermissions(serverId: string): RoomPermissions {
    this.requireAgentRoster(serverId);
    return {
      sendMessages: true,
      attachFiles: true,
      manageMessages: true,
      manageServer: serverId === "server-henosis",
    };
  }

  /** Derive opaque readiness from one submitted seat and the fixture catalog. */
  private fixtureCredentialReadiness(
    seat: AgentSeatDraft,
  ): AgentCredentialReadiness {
    const harness = this.agentCatalog.harnesses.find(
      (candidate) => candidate.id === seat.harnessKey,
    );
    if (!harness || !harness.available) {
      return "unavailable";
    }
    if (harness.credentialMode === "hostSession") {
      return "hostSession";
    }
    if (seat.credentialBindingId) {
      return "ready";
    }
    return harness.credentialMode === "optionalBinding"
      ? "hostSession"
      : "unavailable";
  }

  /** Settle one pending fixture roster as active or as its injected failure. */
  private settlePendingAgentActivation(serverId: string): void {
    const current = this.requireAgentRoster(serverId);
    if (current.runtimeActivation !== "pending") {
      return;
    }
    const failure = this.nextAgentActivationFailures.get(serverId);
    this.nextAgentActivationFailures.delete(serverId);
    const runtimeActivation = failure ? "failed" : "active";
    const settled: AgentRosterSnapshot = {
      ...current,
      activeRevision: failure ? current.activeRevision : current.desiredRevision,
      lastGoodRevision: failure ? current.lastGoodRevision : current.desiredRevision,
      runtimeActivation,
      runtimeErrorCode: failure?.code ?? null,
      runtimeErrorMessage: failure?.message ?? null,
      seats: current.seats.map((seat) => ({
        ...seat,
        settings: cloneAgentSettings(seat.settings),
        runtimeActivation,
      })),
    };
    this.agentRosters.set(serverId, cloneAgentRoster(settled));
  }

  /** Project one cloned public bridge status from authoritative fixture state. */
  private fixtureBridgeStatus(serverId: string): RoomBridgeStatus {
    const roster = this.requireAgentRoster(serverId);
    return {
      paused: this.pausedAgentBridges.get(serverId) ?? false,
      desiredRevision: roster.desiredRevision,
      activeRevision: roster.activeRevision,
      lastGoodRevision: roster.lastGoodRevision,
      runtimeActivation: roster.runtimeActivation,
      runtimeErrorCode: roster.runtimeErrorCode,
      runtimeErrorMessage: roster.runtimeErrorMessage,
    };
  }

  /** Assemble a visibly fixture-backed directory snapshot. */
  private snapshot(): RoomDirectorySnapshot {
    return {
      connection: { ...this.connection },
      rooms: createFixtureRooms(),
      source: "fixture",
      fetchedAt: new Date().toISOString(),
      connected: this.connected,
    };
  }

  /** Return a persistent deterministic history for one known fixture room. */
  private roomHistory(roomId: string): RoomMessage[] {
    const existing = this.histories.get(roomId);
    if (existing) {
      return existing;
    }

    const history = Array.from({ length: 5 }, (_, index) =>
      createFixtureMessage(roomId, index + 1),
    );
    const unreadCount =
      createFixtureRooms().find((room) => room.id === roomId)?.unreadCount ?? 0;
    const initialReadIndex = history.length - unreadCount - 1;
    if (initialReadIndex >= 0) {
      this.readMarkers.set(roomId, history[initialReadIndex].id);
    }
    this.histories.set(roomId, history);
    return history;
  }

  /** Reject room operations after the fixture preview session is disconnected. */
  private requireConnectedSession(): void {
    if (!this.connected) {
      throw new HenosisClientError(
        "connection-required",
        "Connect the browser fixture before opening a room.",
      );
    }
  }

  /** Reject room identifiers outside the explicit public fixture directory. */
  private requireKnownRoom(roomId: string): RoomSummary {
    const room = createFixtureRooms().find(
      (candidate) => candidate.id === roomId,
    );
    if (!room) {
      throw new HenosisClientError(
        "validation",
        "That room is not available in the browser fixture.",
      );
    }
    return room;
  }

  /** Validate and reserve one native-shaped stream capability for this session. */
  private reserveRoomStreamId(streamId: string): void {
    if (!VALID_FIXTURE_STREAM_ID.test(streamId)) {
      throw new HenosisClientError(
        "validation",
        "Start the fixture room with a valid unique stream identifier.",
      );
    }
    if (this.usedRoomStreamIds.has(streamId)) {
      throw new HenosisClientError(
        "validation",
        "That fixture room stream identifier was already used. Start with a new one.",
      );
    }
    if (this.usedRoomStreamIds.size >= MAX_FIXTURE_STREAM_IDS) {
      throw new HenosisClientError(
        "protocol",
        "The fixture reached its room stream limit. Reconnect to continue.",
      );
    }
    this.usedRoomStreamIds.add(streamId);
  }

  /** Return the active generation only when both identifiers match exactly. */
  private requireOpenGeneration(
    roomId: string,
    streamId: string,
  ): OpenFixtureRoom {
    this.requireConnectedSession();
    const openGeneration = this.openGeneration;
    if (
      !openGeneration ||
      openGeneration.roomId !== roomId ||
      openGeneration.streamId !== streamId
    ) {
      throw new HenosisClientError(
        "validation",
        "That fixture room generation is no longer active. Open the room again.",
      );
    }
    return openGeneration;
  }

  /** Mirror native count, opacity, and one-use validation for pending upload IDs. */
  private validatePendingUploadIds(pendingUploadIds: string[]): void {
    if (pendingUploadIds.length > MAX_FIXTURE_ATTACHMENTS) {
      throw new HenosisClientError(
        "validation",
        "A fixture message can include no more than 10 attachments.",
      );
    }
    const uniqueUploadIds = new Set<string>();
    for (const uploadId of pendingUploadIds) {
      if (uploadId.length === 0 || uploadId.trim() !== uploadId) {
        throw new HenosisClientError(
          "validation",
          "A pending fixture attachment identifier was invalid.",
        );
      }
      if (uniqueUploadIds.has(uploadId)) {
        throw new HenosisClientError(
          "validation",
          "A pending fixture attachment can be used only once per message.",
        );
      }
      uniqueUploadIds.add(uploadId);
    }
  }

  /** Return one loaded retained message or reject the bounded operation. */
  private requireLoadedMessage(
    openGeneration: OpenFixtureRoom,
    messageId: string,
  ): RoomMessage {
    if (!openGeneration.loadedMessageIds.has(messageId)) {
      throw new HenosisClientError(
        "validation",
        "That message is outside the loaded fixture window.",
      );
    }
    const message = this.roomHistory(openGeneration.roomId).find(
      (candidate) => candidate.id === messageId,
    );
    if (!message) {
      throw new HenosisClientError(
        "validation",
        "That fixture message is no longer available.",
      );
    }
    return message;
  }

  /** Advance the shared sequence and wrap one committed fixture command value. */
  private commandResult<T>(
    openGeneration: OpenFixtureRoom,
    value: T,
  ): RoomConversationCommandResult<T> {
    openGeneration.sequence += 1;
    return {
      streamId: openGeneration.streamId,
      sequence: openGeneration.sequence,
      value,
    };
  }

  /** Advance the shared sequence and synchronously publish one fixture event. */
  private emit(
    openGeneration: OpenFixtureRoom,
    event: RoomConversationEvent,
  ): void {
    openGeneration.sequence += 1;
    const envelope = {
      streamId: openGeneration.streamId,
      sequence: openGeneration.sequence,
      event,
    };
    for (const listener of this.roomEventListeners) {
      listener(envelope);
    }
  }

  /** Derive the unread divider from a persistent marker and bounded live window. */
  private unreadBoundary(
    roomId: string,
    history: RoomMessage[],
    loadedMessages: RoomMessage[],
  ): RoomUnreadBoundary {
    const firstLoadedId = loadedMessages[0]?.id;
    const firstLoadedIndex = history.findIndex(
      (message) => message.id === firstLoadedId,
    );
    const markerId = this.readMarkers.get(roomId);

    if (markerId) {
      const markerIndex = history.findIndex((message) => message.id === markerId);
      if (markerIndex >= history.length - 1) {
        return { kind: "none" };
      }
      const firstUnreadIndex = Math.max(0, markerIndex + 1);
      if (firstUnreadIndex < firstLoadedIndex) {
        return { kind: "beforeLoadedWindow" };
      }
      return {
        kind: "beforeMessage",
        messageId: history[firstUnreadIndex].id,
      };
    }

    const unreadCount =
      createFixtureRooms().find((room) => room.id === roomId)?.unreadCount ?? 0;
    if (unreadCount === 0) {
      return { kind: "none" };
    }
    const firstUnreadIndex = Math.max(0, history.length - unreadCount);
    if (firstUnreadIndex < firstLoadedIndex) {
      return { kind: "beforeLoadedWindow" };
    }
    return {
      kind: "beforeMessage",
      messageId: history[firstUnreadIndex].id,
    };
  }

  /** Build a same-origin fixture attachment URL from an opaque upload identifier. */
  private attachmentUrl(uploadId: string): string {
    const endpoint = this.connection.endpoint.replace(/\/+$/, "");
    return `${endpoint}/attachments/${encodeURIComponent(uploadId)}`;
  }

  /** Return the next deterministic ISO timestamp for a fixture mutation. */
  private nextTimestamp(): string {
    const timestamp = new Date(
      Date.UTC(2026, 7, 1, 12, this.logicalMinute),
    ).toISOString();
    this.logicalMinute += 1;
    return timestamp;
  }
}
