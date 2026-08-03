/** Native adapter and error-boundary tests for the desktop client contract. */
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  RoomConversationEventEnvelope,
  RoomConversationSnapshot,
} from "../domain/conversation";

/** Hoisted Tauri spies installed before the adapter module is evaluated. */
const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

import { normalizeClientError } from "./henosisClient";
import { TauriHenosisClient } from "./tauriClient";

/** Return the smallest complete room snapshot accepted by the typed boundary. */
function roomSnapshot(): RoomConversationSnapshot {
  return {
    roomId: "room-1",
    streamId: "stream-1",
    lastEventSequence: 0,
    currentUserId: "user-1",
    permissions: {
      sendMessages: true,
      attachFiles: true,
      manageMessages: false,
      manageServer: false,
    },
    unreadBoundary: { kind: "none" },
    page: { messages: [], hasOlder: false },
    connectionStatus: "connected",
  };
}

describe("TauriHenosisClient", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    listenMock.mockReset();
  });

  it("maps every room operation to its exact native command and payload", async () => {
    invokeMock.mockResolvedValue(undefined);
    invokeMock.mockResolvedValueOnce(roomSnapshot());
    const client = new TauriHenosisClient();

    await client.openRoom("room-1", "stream-1");
    await client.loadOlderMessages("room-1", "stream-1", "message-1");
    await client.sendRoomMessage("room-1", "stream-1", "hello", ["upload-1"]);
    await client.editRoomMessage(
      "room-1",
      "stream-1",
      "message-2",
      "updated",
    );
    await client.deleteRoomMessage("room-1", "stream-1", "message-2");
    await client.selectAndUploadRoomAttachments("room-1", "stream-1");
    await client.sendRoomTyping("room-1", "stream-1");
    await client.markRoomRead("room-1", "stream-1", "message-3");
    await client.closeRoom("room-1", "stream-1");

    expect(invokeMock.mock.calls).toEqual([
      ["open_room", { roomId: "room-1", streamId: "stream-1" }],
      [
        "load_older_messages",
        {
          roomId: "room-1",
          streamId: "stream-1",
          beforeMessageId: "message-1",
        },
      ],
      [
        "send_room_message",
        {
          roomId: "room-1",
          streamId: "stream-1",
          content: "hello",
          pendingUploadIds: ["upload-1"],
        },
      ],
      [
        "edit_room_message",
        {
          roomId: "room-1",
          streamId: "stream-1",
          messageId: "message-2",
          content: "updated",
        },
      ],
      [
        "delete_room_message",
        { roomId: "room-1", streamId: "stream-1", messageId: "message-2" },
      ],
      [
        "select_and_upload_room_attachments",
        { roomId: "room-1", streamId: "stream-1" },
      ],
      ["send_room_typing", { roomId: "room-1", streamId: "stream-1" }],
      [
        "mark_room_read",
        { roomId: "room-1", streamId: "stream-1", messageId: "message-3" },
      ],
      ["close_room", { roomId: "room-1", streamId: "stream-1" }],
    ]);
  });

  it("forwards only the fixed-channel event payload and returns native cleanup", async () => {
    const cleanup = vi.fn();
    const listener = vi.fn();
    let dispatch: ((event: { payload: RoomConversationEventEnvelope }) => void) | undefined;
    listenMock.mockImplementation(
      async (
        _eventName: string,
        callback: (event: { payload: RoomConversationEventEnvelope }) => void,
      ) => {
        dispatch = callback;
        return cleanup;
      },
    );
    const client = new TauriHenosisClient();
    const envelope: RoomConversationEventEnvelope = {
      streamId: "stream-1",
      sequence: 1,
      event: {
        type: "connectionChanged",
        data: { roomId: "room-1", status: "reconnecting" },
      },
    };

    const unlisten = await client.subscribeRoomEvents(listener);
    dispatch?.({ payload: envelope });
    unlisten();

    expect(listenMock).toHaveBeenCalledWith(
      "henosis://room-conversation",
      expect.any(Function),
    );
    expect(listener).toHaveBeenCalledExactlyOnceWith(envelope);
    expect(cleanup).toHaveBeenCalledOnce();
  });

  it("redacts raw invoke and event-listen failures", async () => {
    const client = new TauriHenosisClient();
    invokeMock.mockRejectedValueOnce("password=do-not-render");
    listenMock.mockRejectedValueOnce("token=do-not-render");

    await expect(client.openRoom("room-1", "stream-1")).rejects.toMatchObject({
      kind: "unknown",
      message: expect.not.stringContaining("do-not-render"),
    });
    await expect(client.subscribeRoomEvents(vi.fn())).rejects.toMatchObject({
      kind: "unknown",
      message: expect.not.stringContaining("do-not-render"),
    });
  });
});

describe("normalizeClientError", () => {
  it("preserves structured native recovery guidance", () => {
    const result = normalizeClientError({
      kind: "network",
      message: "Check the Rift endpoint.",
    });

    expect(result.kind).toBe("network");
    expect(result.message).toBe("Check the Rift endpoint.");
  });

  it("parses structured Tauri errors serialized as JSON", () => {
    const result = normalizeClientError(
      JSON.stringify({
        kind: "authentication",
        message: "Sign in again.",
      }),
    );

    expect(result.kind).toBe("authentication");
    expect(result.message).toBe("Sign in again.");
  });

  it("redacts unstructured rejection strings", () => {
    const result = normalizeClientError(
      "transport failed with password=do-not-render",
    );

    expect(result.kind).toBe("unknown");
    expect(result.message).not.toContain("do-not-render");
    expect(result.message).toContain("reconnect to Rift");
  });
});
