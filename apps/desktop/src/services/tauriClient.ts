/** Tauri command adapter for the production Henosis desktop runtime. */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  AgentCapabilityCatalog,
  AgentRosterSnapshot,
  ApplyAgentRosterRequest,
  OwnedAgentIdentity,
  RoomBridgeStatus,
} from "../domain/agentControl";
import type {
  MessagePage,
  PendingRoomAttachment,
  RoomConversationCommandResult,
  RoomConversationEventEnvelope,
  RoomConversationSnapshot,
  RoomMessage,
  RoomPermissions,
} from "../domain/conversation";
import type {
  BootstrapResult,
  HenosisClient,
  RiftConnectionInput,
  RoomDirectorySnapshot,
  RoomEventListener,
} from "./henosisClient";
import { normalizeClientError } from "./henosisClient";

/** Fixed native channel carrying sanitized room conversation envelopes. */
const ROOM_CONVERSATION_EVENT = "henosis://room-conversation";

/** Client whose calls terminate at the Rust process instead of Rift directly. */
export class TauriHenosisClient implements HenosisClient {
  /** Load saved profile, cache, and any active native session. */
  async bootstrap(): Promise<BootstrapResult> {
    return this.invokeCommand<BootstrapResult>("bootstrap");
  }

  /** Pass credentials once to Rust and receive only sanitized room data. */
  async connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot> {
    return this.invokeCommand<RoomDirectorySnapshot>("connect_rift", { input });
  }

  /** Refresh room summaries using the token held in native process state. */
  async refresh(): Promise<RoomDirectorySnapshot> {
    return this.invokeCommand<RoomDirectorySnapshot>("get_room_directory");
  }

  /** Clear native token state and request Rift refresh-session cleanup. */
  async disconnect(): Promise<void> {
    await this.invokeCommand<void>("disconnect_rift");
  }

  /** List persistent agent identities owned by the signed-in Rift human. */
  async getMyAgents(): Promise<OwnedAgentIdentity[]> {
    return this.invokeCommand<OwnedAgentIdentity[]>("get_my_agents");
  }

  /** Create one persistent agent identity owned by the signed-in Rift human. */
  async createMyAgent(
    username: string,
    displayName: string | null,
  ): Promise<OwnedAgentIdentity> {
    return this.invokeCommand<OwnedAgentIdentity>("create_my_agent", {
      username,
      displayName,
    });
  }

  /** Claim one known imported agent identity for the signed-in Rift human. */
  async claimAgent(agentIdentityId: string): Promise<OwnedAgentIdentity> {
    return this.invokeCommand<OwnedAgentIdentity>("claim_agent", {
      agentIdentityId,
    });
  }

  /** Load one deployment-discovered execution capability catalog. */
  async getAgentCapabilities(serverId: string): Promise<AgentCapabilityCatalog> {
    return this.invokeCommand<AgentCapabilityCatalog>("get_agent_capabilities", {
      serverId,
    });
  }

  /** Load authoritative permissions for the signed-in room member. */
  async getRoomPermissions(serverId: string): Promise<RoomPermissions> {
    return this.invokeCommand<RoomPermissions>("get_room_permissions", {
      serverId,
    });
  }

  /** Load the authoritative desired room roster and activation state. */
  async getRoomAgentRoster(serverId: string): Promise<AgentRosterSnapshot> {
    return this.invokeCommand<AgentRosterSnapshot>("get_room_agent_roster", {
      serverId,
    });
  }

  /** Apply one optimistic complete room roster replacement. */
  async applyRoomAgentRoster(
    serverId: string,
    update: ApplyAgentRosterRequest,
  ): Promise<AgentRosterSnapshot> {
    return this.invokeCommand<AgentRosterSnapshot>("apply_room_agent_roster", {
      serverId,
      update,
    });
  }

  /** Load public pause and activation state for one room bridge. */
  async getRoomBridgeStatus(serverId: string): Promise<RoomBridgeStatus> {
    return this.invokeCommand<RoomBridgeStatus>("get_room_bridge_status", {
      serverId,
    });
  }

  /** Pause autonomous activity for one room bridge. */
  async pauseRoomBridge(serverId: string): Promise<RoomBridgeStatus> {
    return this.invokeCommand<RoomBridgeStatus>("pause_room_bridge", {
      serverId,
    });
  }

  /** Resume autonomous activity for one room bridge. */
  async resumeRoomBridge(serverId: string): Promise<RoomBridgeStatus> {
    return this.invokeCommand<RoomBridgeStatus>("resume_room_bridge", {
      serverId,
    });
  }

  /** Retry the current desired roster without creating another revision. */
  async reconcileRoomBridge(serverId: string): Promise<AgentRosterSnapshot> {
    return this.invokeCommand<AgentRosterSnapshot>("reconcile_room_bridge", {
      serverId,
    });
  }

  /** Open one exact room generation through native permissions and reconciliation. */
  async openRoom(
    roomId: string,
    streamId: string,
  ): Promise<RoomConversationSnapshot> {
    return this.invokeCommand<RoomConversationSnapshot>("open_room", {
      roomId,
      streamId,
    });
  }

  /** Close only the exact native room generation identified by the caller. */
  async closeRoom(roomId: string, streamId: string): Promise<void> {
    await this.invokeCommand<void>("close_room", { roomId, streamId });
  }

  /** Load one bounded page before the current oldest native message. */
  async loadOlderMessages(
    roomId: string,
    streamId: string,
    beforeMessageId: string,
  ): Promise<RoomConversationCommandResult<MessagePage>> {
    return this.invokeCommand<RoomConversationCommandResult<MessagePage>>(
      "load_older_messages",
      { roomId, streamId, beforeMessageId },
    );
  }

  /** Send text and opaque staged upload identifiers through native state. */
  async sendRoomMessage(
    roomId: string,
    streamId: string,
    content: string,
    pendingUploadIds: string[],
  ): Promise<RoomConversationCommandResult<RoomMessage | null>> {
    return this.invokeCommand<RoomConversationCommandResult<RoomMessage | null>>(
      "send_room_message",
      { roomId, streamId, content, pendingUploadIds },
    );
  }

  /** Edit one currently loaded room message through the native boundary. */
  async editRoomMessage(
    roomId: string,
    streamId: string,
    messageId: string,
    content: string,
  ): Promise<RoomConversationCommandResult<RoomMessage | null>> {
    return this.invokeCommand<RoomConversationCommandResult<RoomMessage | null>>(
      "edit_room_message",
      { roomId, streamId, messageId, content },
    );
  }

  /** Delete one currently loaded room message through the native boundary. */
  async deleteRoomMessage(
    roomId: string,
    streamId: string,
    messageId: string,
  ): Promise<RoomConversationCommandResult<string>> {
    return this.invokeCommand<RoomConversationCommandResult<string>>(
      "delete_room_message",
      { roomId, streamId, messageId },
    );
  }

  /** Let native code pick and stage files without returning local paths. */
  async selectAndUploadRoomAttachments(
    roomId: string,
    streamId: string,
  ): Promise<RoomConversationCommandResult<PendingRoomAttachment[]>> {
    return this.invokeCommand<
      RoomConversationCommandResult<PendingRoomAttachment[]>
    >("select_and_upload_room_attachments", { roomId, streamId });
  }

  /** Queue one coalesced typing signal through the current gateway actor. */
  async sendRoomTyping(roomId: string, streamId: string): Promise<void> {
    await this.invokeCommand<void>("send_room_typing", { roomId, streamId });
  }

  /** Persist one monotonic read marker inside native storage. */
  async markRoomRead(
    roomId: string,
    streamId: string,
    messageId: string,
  ): Promise<void> {
    await this.invokeCommand<void>("mark_room_read", {
      roomId,
      streamId,
      messageId,
    });
  }

  /** Forward sanitized native event payloads and return native cleanup. */
  async subscribeRoomEvents(listener: RoomEventListener): Promise<UnlistenFn> {
    try {
      return await listen<RoomConversationEventEnvelope>(
        ROOM_CONVERSATION_EVENT,
        (event) => listener(event.payload),
      );
    } catch (error) {
      throw normalizeClientError(error);
    }
  }

  /** Invoke one command while normalizing every rejected boundary value. */
  private async invokeCommand<T>(
    command: string,
    payload?: Record<string, unknown>,
  ): Promise<T> {
    try {
      return payload === undefined
        ? await invoke<T>(command)
        : await invoke<T>(command, payload);
    } catch (error) {
      throw normalizeClientError(error);
    }
  }
}
