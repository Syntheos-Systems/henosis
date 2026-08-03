/** Deterministic projection tests for the native room conversation contract. */
import { describe, expect, it } from "vitest";
import {
  TYPING_EXPIRY_MS,
  advanceConversationCommand,
  applyConversationDeleteResult,
  applyConversationEvent,
  applyConversationMessageResult,
  applyConversationOlderPageResult,
  expireConversationTyping,
  markConversationReadThrough,
  replaceConversationSnapshot,
  setConversationLiveEdge,
} from "./conversation";
import type {
  BufferedConversationEvent,
  RoomConversationCommandResult,
  RoomConversationEvent,
  RoomConversationEventEnvelope,
  RoomConversationSnapshot,
  RoomMessage,
} from "./conversation";

/** Stable room identifier shared by reducer fixtures. */
const ROOM_ID = "room-1";

/** Stable generation token shared by reducer fixtures. */
const STREAM_ID = "stream-conversation-0001";

/** Build one complete sanitized message with narrow per-test overrides. */
function message(
  id: string,
  createdAt: string,
  overrides: Partial<RoomMessage> = {},
): RoomMessage {
  return {
    id,
    roomId: ROOM_ID,
    authorId: "user-1",
    authorUsername: "operator",
    authorDisplayName: "Operator",
    authorAvatarUrl: null,
    content: id,
    editedAt: null,
    createdAt,
    messageType: "user",
    attachments: [],
    ...overrides,
  };
}

/** Build one native opening snapshot with deterministic defaults. */
function snapshot(
  overrides: Partial<RoomConversationSnapshot> = {},
): RoomConversationSnapshot {
  return {
    roomId: ROOM_ID,
    streamId: STREAM_ID,
    lastEventSequence: 0,
    currentUserId: "user-1",
    permissions: {
      sendMessages: true,
      attachFiles: true,
      manageMessages: false,
      manageServer: false,
    },
    unreadBoundary: { kind: "none" },
    page: {
      messages: [],
      hasOlder: false,
    },
    connectionStatus: "connected",
    ...overrides,
  };
}

/** Wrap one native event in its generation and shared sequence. */
function envelope(
  sequence: number,
  event: RoomConversationEvent,
  streamId = STREAM_ID,
): RoomConversationEventEnvelope {
  return {
    streamId,
    sequence,
    event,
  };
}

/** Wrap one command value in the native shared sequence contract. */
function commandResult<T>(
  sequence: number,
  value: T,
  streamId = STREAM_ID,
): RoomConversationCommandResult<T> {
  return {
    streamId,
    sequence,
    value,
  };
}

describe("replaceConversationSnapshot", () => {
  it("replaces generations, normalizes newest duplicates, and drains buffered events", () => {
    const previous = replaceConversationSnapshot(
      null,
      snapshot({
        streamId: "stream-old-generation",
        page: {
          messages: [message("old-generation", "2026-08-03T11:00:00Z")],
          hasOlder: true,
        },
      }),
    );
    const duplicateOld = message("message-1", "2026-08-03T12:00:00Z", {
      content: "old",
    });
    const duplicateNew = message("message-1", "2026-08-03T12:00:00Z", {
      content: "edited",
      editedAt: "2026-08-03T12:01:00Z",
    });
    const buffered: BufferedConversationEvent[] = [
      {
        envelope: envelope(3, {
          type: "messageCreate",
          data: {
            roomId: ROOM_ID,
            message: message("message-3", "2026-08-03T12:03:00Z"),
          },
        }),
        receivedAt: 300,
      },
      {
        envelope: envelope(
          2,
          {
            type: "messageCreate",
            data: {
              roomId: ROOM_ID,
              message: message("wrong-stream", "2026-08-03T12:02:00Z"),
            },
          },
          "stream-wrong-generation",
        ),
        receivedAt: 200,
      },
      {
        envelope: envelope(2, {
          type: "messageCreate",
          data: {
            roomId: ROOM_ID,
            message: message("message-2", "2026-08-03T12:02:00Z"),
          },
        }),
        receivedAt: 200,
      },
      {
        envelope: envelope(1, {
          type: "messageDelete",
          data: {
            roomId: ROOM_ID,
            messageId: "message-1",
          },
        }),
        receivedAt: 100,
      },
    ];

    const state = replaceConversationSnapshot(
      previous,
      snapshot({
        lastEventSequence: 1,
        page: {
          messages: [
            duplicateNew,
            message("precision-newer", "2026-08-03T11:58:00.000000002Z"),
            message("message-a", "2026-08-03T11:59:00Z"),
            message("message-Z", "2026-08-03T11:59:00Z"),
            message("precision-older", "2026-08-03T11:58:00.000000001Z"),
            duplicateOld,
          ],
          hasOlder: true,
        },
      }),
      buffered,
    );

    expect(state.streamId).toBe(STREAM_ID);
    expect(state.lastEventSequence).toBe(3);
    expect(state.messages.map((entry) => entry.id)).toEqual([
      "precision-older",
      "precision-newer",
      "message-Z",
      "message-a",
      "message-1",
      "message-2",
      "message-3",
    ]);
    expect(
      state.messages.find((entry) => entry.id === "message-1")?.content,
    ).toBe("edited");
    expect(state.messages.some((entry) => entry.id === "old-generation")).toBe(
      false,
    );
    expect(state.messages.some((entry) => entry.id === "wrong-stream")).toBe(
      false,
    );
  });
});

describe("applyConversationEvent", () => {
  it("deduplicates creates and ignores stale, wrong-room, and wrong-stream events", () => {
    const initial = replaceConversationSnapshot(null, snapshot());
    const created = applyConversationEvent(
      initial,
      envelope(1, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-1", "2026-08-03T12:00:00Z"),
        },
      }),
      100,
    );
    const duplicate = applyConversationEvent(
      created,
      envelope(1, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-1", "2026-08-03T12:00:00Z", {
            content: "must not replace",
          }),
        },
      }),
      200,
    );
    const wrongRoom = applyConversationEvent(
      duplicate,
      envelope(2, {
        type: "messageDelete",
        data: {
          roomId: "room-other",
          messageId: "message-1",
        },
      }),
      300,
    );
    const updated = applyConversationEvent(
      wrongRoom,
      envelope(2, {
        type: "messageUpdate",
        data: {
          roomId: ROOM_ID,
          messageId: "message-1",
          content: "updated",
          editedAt: "2026-08-03T12:01:00Z",
        },
      }),
      400,
    );
    const wrongStream = applyConversationEvent(
      updated,
      envelope(
        3,
        {
          type: "messageDelete",
          data: {
            roomId: ROOM_ID,
            messageId: "message-1",
          },
        },
        "stream-other-generation",
      ),
      500,
    );

    expect(duplicate).toBe(created);
    expect(wrongRoom).toBe(created);
    expect(wrongStream).toBe(updated);
    expect(updated.messages).toHaveLength(1);
    expect(updated.messages[0].content).toBe("updated");
    expect(updated.lastEventSequence).toBe(2);
  });

  it("retains an update before create and rejects equal or older full versions", () => {
    const initial = replaceConversationSnapshot(null, snapshot());
    const updatedFirst = applyConversationEvent(
      initial,
      envelope(1, {
        type: "messageUpdate",
        data: {
          roomId: ROOM_ID,
          messageId: "message-race",
          content: "edit arrived first",
          editedAt: "2026-08-03T12:02:00Z",
        },
      }),
      100,
    );
    const createdLater = applyConversationEvent(
      updatedFirst,
      envelope(2, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-race", "2026-08-03T12:00:00Z"),
        },
      }),
      200,
    );
    const equalVersion = applyConversationEvent(
      createdLater,
      envelope(3, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-race", "2026-08-03T12:00:00Z", {
            content: "equal timestamp must lose",
            editedAt: "2026-08-03T12:02:00Z",
          }),
        },
      }),
      300,
    );
    const newerVersion = applyConversationEvent(
      equalVersion,
      envelope(4, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-race", "2026-08-03T12:00:00Z", {
            content: "newest full value",
            editedAt: "2026-08-03T12:03:00Z",
          }),
        },
      }),
      400,
    );
    const fractionalOlder = applyConversationEvent(
      newerVersion,
      envelope(5, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-race", "2026-08-03T12:00:00Z", {
            content: "first nanosecond edit",
            editedAt: "2026-08-03T12:03:00.000000001Z",
          }),
        },
      }),
      500,
    );
    const fractionalNewer = applyConversationEvent(
      fractionalOlder,
      envelope(6, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-race", "2026-08-03T12:00:00Z", {
            content: "second nanosecond edit",
            editedAt: "2026-08-03T12:03:00.000000002Z",
          }),
        },
      }),
      600,
    );

    expect(createdLater.messages[0]).toMatchObject({
      content: "edit arrived first",
      editedAt: "2026-08-03T12:02:00Z",
    });
    expect(equalVersion.messages[0].content).toBe("edit arrived first");
    expect(newerVersion.messages[0].content).toBe("newest full value");
    expect(fractionalOlder.messages[0].content).toBe("first nanosecond edit");
    expect(fractionalNewer.messages[0].content).toBe("second nanosecond edit");
  });

  it("tombstones deletes so duplicate creates and pages cannot resurrect messages", () => {
    const initial = replaceConversationSnapshot(null, snapshot());
    const deleted = applyConversationEvent(
      initial,
      envelope(1, {
        type: "messageDelete",
        data: {
          roomId: ROOM_ID,
          messageId: "message-deleted",
        },
      }),
      100,
    );
    const duplicateCreate = applyConversationEvent(
      deleted,
      envelope(2, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-deleted", "2026-08-03T12:00:00Z"),
        },
      }),
      200,
    );
    const duplicatePage = applyConversationOlderPageResult(
      duplicateCreate,
      commandResult(3, {
        messages: [message("message-deleted", "2026-08-03T12:00:00Z")],
        hasOlder: false,
      }),
    );

    expect(duplicatePage.messages).toEqual([]);
    expect(duplicatePage.deletedMessageIds.has("message-deleted")).toBe(true);
    expect(duplicatePage.lastEventSequence).toBe(3);
  });
});

describe("shared event and command sequencing", () => {
  it("buffers gaps, drains after the missing command, and prepends older pages", () => {
    const initial = replaceConversationSnapshot(null, snapshot());
    const eventFirst = applyConversationEvent(
      initial,
      envelope(2, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-2", "2026-08-03T12:02:00Z"),
        },
      }),
      200,
    );

    expect(eventFirst.lastEventSequence).toBe(0);
    expect(eventFirst.pendingTransitions.size).toBe(1);

    const commandArrives = applyConversationMessageResult(
      eventFirst,
      commandResult(
        1,
        message("message-1", "2026-08-03T12:01:00Z"),
      ),
    );
    const older = applyConversationOlderPageResult(
      commandArrives,
      commandResult(3, {
        messages: [message("message-0", "2026-08-03T12:00:00Z")],
        hasOlder: false,
      }),
    );
    const laterEventFirst = applyConversationEvent(
      older,
      envelope(5, {
        type: "presenceUpdate",
        data: {
          roomId: ROOM_ID,
          userId: "user-2",
          status: "online",
        },
      }),
      500,
    );
    const advanced = advanceConversationCommand(
      laterEventFirst,
      commandResult(4, [
        {
          uploadId: "upload-1",
          filename: "report.txt",
          contentType: "text/plain",
          sizeBytes: 12,
        },
      ]),
    );

    expect(commandArrives.lastEventSequence).toBe(2);
    expect(commandArrives.pendingTransitions.size).toBe(0);
    expect(older.messages.map((entry) => entry.id)).toEqual([
      "message-0",
      "message-1",
      "message-2",
    ]);
    expect(older.hasOlder).toBe(false);
    expect(advanced.lastEventSequence).toBe(5);
    expect(advanced.presenceByUserId.get("user-2")).toBe("online");
  });
});

describe("conversation reconciliation", () => {
  it("merges forward pages and replaces bounded fallback windows without regressions", () => {
    const initial = replaceConversationSnapshot(
      null,
      snapshot({
        page: {
          messages: [
            message("remove-on-replace", "2026-08-03T12:00:00Z"),
            message("keep-newer", "2026-08-03T12:01:00Z", {
              content: "newer retained",
              editedAt: "2026-08-03T12:05:00Z",
            }),
          ],
          hasOlder: true,
        },
      }),
    );
    const away = setConversationLiveEdge(initial, false);
    const merged = applyConversationEvent(
      away,
      envelope(1, {
        type: "reconciliation",
        data: {
          roomId: ROOM_ID,
          page: {
            messages: [
              message("forward", "2026-08-03T12:06:00Z", {
                authorId: "user-2",
                authorUsername: "agent",
              }),
            ],
            hasOlder: false,
          },
          replaceLiveWindow: false,
        },
      }),
      100,
    );
    const replaced = applyConversationEvent(
      merged,
      envelope(2, {
        type: "reconciliation",
        data: {
          roomId: ROOM_ID,
          page: {
            messages: [
              message("keep-newer", "2026-08-03T12:01:00Z", {
                content: "stale fallback",
                editedAt: "2026-08-03T12:04:00Z",
              }),
              message("fallback-new", "2026-08-03T12:07:00Z", {
                authorId: "user-2",
                authorUsername: "agent",
              }),
            ],
            hasOlder: false,
          },
          replaceLiveWindow: true,
        },
      }),
      200,
    );

    expect(merged.hasOlder).toBe(true);
    expect(merged.messages.map((entry) => entry.id)).toEqual([
      "remove-on-replace",
      "keep-newer",
      "forward",
    ]);
    expect(merged.unreadBoundary).toEqual({
      kind: "beforeMessage",
      messageId: "forward",
    });
    expect(replaced.hasOlder).toBe(false);
    expect(replaced.messages.map((entry) => entry.id)).toEqual([
      "keep-newer",
      "fallback-new",
    ]);
    expect(replaced.messages[0].content).toBe("newer retained");
    expect(replaced.unreadBoundary).toEqual({
      kind: "beforeMessage",
      messageId: "fallback-new",
    });
  });
});

describe("ephemeral conversation state", () => {
  it("moves the unread divider and preserves explicit live-edge state", () => {
    const initial = replaceConversationSnapshot(
      null,
      snapshot({
        unreadBoundary: {
          kind: "beforeMessage",
          messageId: "message-b",
        },
        page: {
          messages: [
            message("message-a", "2026-08-03T12:00:00Z"),
            message("message-b", "2026-08-03T12:01:00Z"),
            message("message-c", "2026-08-03T12:02:00Z"),
          ],
          hasOlder: false,
        },
      }),
    );
    const away = setConversationLiveEdge(initial, false);
    const readThrough = markConversationReadThrough(away, "message-b");
    const ignoredRegression = markConversationReadThrough(
      readThrough,
      "message-a",
    );
    const deletedBoundary = applyConversationDeleteResult(
      readThrough,
      commandResult(1, "message-c"),
    );
    const newMessage = applyConversationEvent(
      deletedBoundary,
      envelope(2, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-d", "2026-08-03T12:03:00Z", {
            authorId: "user-2",
            authorUsername: "agent",
          }),
        },
      }),
      200,
    );
    const ownMessage = applyConversationEvent(
      newMessage,
      envelope(3, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-e", "2026-08-03T12:04:00Z"),
        },
      }),
      300,
    );

    expect(ignoredRegression).toBe(readThrough);
    expect(readThrough.unreadBoundary).toEqual({
      kind: "beforeMessage",
      messageId: "message-c",
    });
    expect(deletedBoundary.unreadBoundary).toEqual({ kind: "none" });
    expect(newMessage.atLiveEdge).toBe(false);
    expect(newMessage.unreadBoundary).toEqual({
      kind: "beforeMessage",
      messageId: "message-d",
    });
    expect(ownMessage.unreadBoundary).toEqual(newMessage.unreadBoundary);
    expect(setConversationLiveEdge(ownMessage, true).atLiveEdge).toBe(true);
  });

  it("keeps a live arrival unread until an explicit visibility-confirmed read", () => {
    const initial = replaceConversationSnapshot(null, snapshot());
    const arrived = applyConversationEvent(
      initial,
      envelope(1, {
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-live", "2026-08-03T12:05:00Z", {
            authorId: "user-2",
            authorUsername: "agent",
          }),
        },
      }),
      100,
    );

    expect(arrived.atLiveEdge).toBe(true);
    expect(arrived.unreadBoundary).toEqual({
      kind: "beforeMessage",
      messageId: "message-live",
    });
    expect(
      markConversationReadThrough(arrived, "message-live").unreadBoundary,
    ).toEqual({ kind: "none" });
  });

  it("expires typing deterministically and replaces presence, upload, and connection state", () => {
    const initial = replaceConversationSnapshot(null, snapshot());
    const typing = applyConversationEvent(
      initial,
      envelope(1, {
        type: "typingStart",
        data: {
          roomId: ROOM_ID,
          userId: "user-2",
          username: "agent",
        },
      }),
      1_000,
    );
    const presence = applyConversationEvent(
      typing,
      envelope(2, {
        type: "presenceUpdate",
        data: {
          roomId: ROOM_ID,
          userId: "user-2",
          status: "online",
        },
      }),
      2_000,
    );
    const uploadStarted = applyConversationEvent(
      presence,
      envelope(3, {
        type: "uploadProgress",
        data: {
          roomId: ROOM_ID,
          transferId: "transfer-1",
          filename: "report.txt",
          bytesSent: 0,
          totalBytes: 100,
        },
      }),
      3_000,
    );
    const uploadFinished = applyConversationEvent(
      uploadStarted,
      envelope(4, {
        type: "uploadProgress",
        data: {
          roomId: ROOM_ID,
          transferId: "transfer-1",
          filename: "report.txt",
          bytesSent: 100,
          totalBytes: 100,
        },
      }),
      4_000,
    );
    const reconnecting = applyConversationEvent(
      uploadFinished,
      envelope(5, {
        type: "connectionChanged",
        data: {
          roomId: ROOM_ID,
          status: "reconnecting",
        },
      }),
      5_000,
    );

    expect(
      expireConversationTyping(
        reconnecting,
        1_000 + TYPING_EXPIRY_MS - 1,
      ).typingByUserId.size,
    ).toBe(1);
    expect(
      expireConversationTyping(
        reconnecting,
        1_000 + TYPING_EXPIRY_MS,
      ).typingByUserId.size,
    ).toBe(0);
    expect(reconnecting.presenceByUserId.get("user-2")).toBe("online");
    expect(reconnecting.uploadsByTransferId.get("transfer-1")).toMatchObject({
      filename: "report.txt",
      bytesSent: 100,
      totalBytes: 100,
    });
    expect(reconnecting.connectionStatus).toBe("reconnecting");
    expect(JSON.stringify(reconnecting)).not.toMatch(
      /localPath|selectedPath|filesystemPath/i,
    );
  });
});
