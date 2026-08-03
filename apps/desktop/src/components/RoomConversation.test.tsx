/** Interaction coverage for the generation-scoped Rift room conversation UI. */
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { TYPING_EXPIRY_MS } from "../domain/conversation";
import type {
  MessagePage,
  PendingRoomAttachment,
  RoomConversationCommandResult,
  RoomConversationEvent,
  RoomConversationEventEnvelope,
  RoomConversationSnapshot,
  RoomMessage,
} from "../domain/conversation";
import { FixtureHenosisClient } from "../services/fixtureClient";
import { HenosisClientError } from "../services/henosisClient";
import type { HenosisClient, RoomEventListener } from "../services/henosisClient";
import { RoomConversation } from "./RoomConversation";

/** Stable room identifier shared by the component fixtures. */
const ROOM_ID = "room-conversation-test";

/** Scroll geometry that can be changed around an asynchronous history prepend. */
interface MutableScrollGeometry {
  /** Current synthetic scroll height. */
  height: number;
  /** Current synthetic viewport height. */
  viewport: number;
  /** Current synthetic vertical offset. */
  top: number;
}

/** Controllable client boundary used by interaction tests. */
interface ConversationHarness {
  /** Fully typed fake adapter passed to the component. */
  client: HenosisClient;
  /** Deliver one ordered native event to the current subscriber. */
  emit(event: RoomConversationEvent): void;
  /** Resolve a deliberately suspended opening snapshot. */
  releaseOpen(): void;
  /** Spy invoked when the event subscription is cleaned up. */
  unlisten: ReturnType<typeof vi.fn>;
}

/** Build one complete sanitized message with overridable presentation fields. */
function message(
  id: string,
  createdAt: string,
  overrides: Partial<RoomMessage> = {},
): RoomMessage {
  return {
    id,
    roomId: ROOM_ID,
    authorId: "user-current",
    authorUsername: "ada",
    authorDisplayName: "Ada Lovelace",
    authorAvatarUrl: null,
    content: `Message ${id}`,
    editedAt: null,
    createdAt,
    messageType: "user",
    attachments: [],
    ...overrides,
  };
}

/** Return the newest-page fixture spanning every visual message treatment. */
function newestMessages(): RoomMessage[] {
  return [
    message("message-human", "2026-08-01T12:00:00Z", {
      content: "A human opening thought.",
    }),
    message("message-agent", "2026-08-01T12:01:00Z", {
      authorId: "agent-atlas",
      authorUsername: "atlas",
      authorDisplayName: "Atlas",
      content: "[AGREE] Ship the bounded change.",
      messageType: "agent",
    }),
    message("message-system", "2026-08-01T12:02:00Z", {
      authorId: "system",
      authorUsername: "system",
      authorDisplayName: null,
      content: "Atlas joined the room.",
      messageType: "system",
    }),
    message("message-stimulus", "2026-08-01T12:03:00Z", {
      authorId: "agent-atlas",
      authorUsername: "atlas",
      authorDisplayName: "Atlas",
      content: "[STIMULUS] A repository revision arrived.",
      messageType: "stimulus",
    }),
  ];
}

/** Build the opening projection returned by the fake native adapter. */
function snapshot(
  streamId: string,
  overrides: Partial<RoomConversationSnapshot> = {},
): RoomConversationSnapshot {
  return {
    roomId: ROOM_ID,
    streamId,
    lastEventSequence: 0,
    currentUserId: "user-current",
    permissions: {
      sendMessages: true,
      attachFiles: true,
      manageMessages: true,
      manageServer: false,
    },
    unreadBoundary: {
      kind: "beforeMessage",
      messageId: "message-agent",
    },
    page: {
      messages: newestMessages(),
      hasOlder: true,
    },
    connectionStatus: "connected",
    ...overrides,
  };
}

/** Create a fake client that preserves the shared command and event sequence. */
function createHarness(options: { holdOpen?: boolean } = {}): ConversationHarness {
  let listener: RoomEventListener | null = null;
  let activeStreamId = "";
  let sequence = 1;
  let uploadOrdinal = 0;
  let releaseOpen: (() => void) | null = null;
  const unlisten = vi.fn();

  /** Wrap a fake command value in the active generation and sequence. */
  function commandResult<T>(value: T): RoomConversationCommandResult<T> {
    const result = { streamId: activeStreamId, sequence, value };
    sequence += 1;
    return result;
  }

  const client: HenosisClient = {
    bootstrap: vi.fn(),
    connect: vi.fn(),
    refresh: vi.fn(),
    disconnect: vi.fn(),
    openRoom: vi.fn(async (_roomId: string, streamId: string) => {
      activeStreamId = streamId;
      if (options.holdOpen) {
        await new Promise<void>((resolve) => {
          releaseOpen = resolve;
        });
      }
      return snapshot(streamId);
    }),
    closeRoom: vi.fn().mockResolvedValue(undefined),
    loadOlderMessages: vi.fn(async () =>
      commandResult<MessagePage>({
        messages: [
          message("message-older", "2026-08-01T11:59:00Z", {
            content: "An explicitly loaded older message.",
          }),
        ],
        hasOlder: false,
      }),
    ),
    sendRoomMessage: vi.fn(
      async (
        _roomId: string,
        _streamId: string,
        content: string,
        pendingUploadIds: string[],
      ) =>
        commandResult<RoomMessage | null>(
          message(`message-sent-${sequence}`, "2026-08-01T12:10:00Z", {
            content,
            attachments: pendingUploadIds.map((uploadId, index) => ({
              id: `attachment-${index + 1}`,
              filename: `${uploadId}.txt`,
              url: `https://rift.example.test/attachments/${uploadId}`,
              contentType: "text/plain",
              sizeBytes: 128,
            })),
          }),
        ),
    ),
    editRoomMessage: vi.fn(
      async (
        _roomId: string,
        _streamId: string,
        messageId: string,
        content: string,
      ) =>
        commandResult<RoomMessage | null>(
          message(messageId, "2026-08-01T12:00:00Z", {
            content,
            editedAt: "2026-08-01T12:11:00Z",
          }),
        ),
    ),
    deleteRoomMessage: vi.fn(async (_roomId, _streamId, messageId) =>
      commandResult(messageId),
    ),
    selectAndUploadRoomAttachments: vi.fn(async () => {
      uploadOrdinal += 1;
      const pending = {
        uploadId: `upload-${uploadOrdinal}`,
        filename: `notes-${uploadOrdinal}.txt`,
        contentType: "text/plain",
        sizeBytes: 128,
        localPath: "NATIVE_PATH_SENTINEL",
      } as PendingRoomAttachment;
      return commandResult([pending]);
    }),
    sendRoomTyping: vi.fn().mockResolvedValue(undefined),
    markRoomRead: vi.fn().mockResolvedValue(undefined),
    subscribeRoomEvents: vi.fn(async (nextListener: RoomEventListener) => {
      listener = nextListener;
      return unlisten;
    }),
  };

  return {
    client,
    emit(event) {
      if (listener === null || activeStreamId.length === 0) {
        throw new Error("The fake room subscription is not active.");
      }
      listener({ streamId: activeStreamId, sequence, event });
      sequence += 1;
    },
    releaseOpen() {
      releaseOpen?.();
    },
    unlisten,
  };
}

/** Install mutable scroll metrics on the rendered timeline element. */
function installScrollGeometry(
  element: HTMLElement,
  geometry: MutableScrollGeometry,
): void {
  Object.defineProperties(element, {
    scrollHeight: {
      configurable: true,
      get: () => geometry.height,
    },
    clientHeight: {
      configurable: true,
      get: () => geometry.viewport,
    },
    scrollTop: {
      configurable: true,
      get: () => geometry.top,
      set: (value: number) => {
        geometry.top = value;
      },
    },
  });
  element.scrollTo = vi.fn((options?: ScrollToOptions | number) => {
    geometry.top =
      typeof options === "number" ? options : (options?.top ?? geometry.top);
  });
}

/** Restore real clocks even when a typing-expiry assertion fails. */
afterEach(() => vi.useRealTimers());

describe("RoomConversation", () => {
  it("buffers opening events and renders the newest page with structural identity", async () => {
    const harness = createHarness({ holdOpen: true });
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);

    expect(screen.getByRole("status")).toHaveTextContent(
      "Loading room conversation",
    );
    await waitFor(() => {
      expect(harness.client.openRoom).toHaveBeenCalledOnce();
    });
    expect(
      vi.mocked(harness.client.subscribeRoomEvents).mock.invocationCallOrder[0],
    ).toBeLessThan(
      vi.mocked(harness.client.openRoom).mock.invocationCallOrder[0],
    );

    act(() => {
      harness.emit({
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-buffered", "2026-08-01T12:04:00Z", {
            authorId: "agent-atlas",
            authorUsername: "atlas",
            authorDisplayName: "Atlas",
            content: "[PASS]",
            messageType: "agent",
          }),
        },
      });
      harness.releaseOpen();
    });

    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });
    const renderedIds = [...timeline.querySelectorAll("[data-message-id]")].map(
      (element) => element.getAttribute("data-message-id"),
    );
    expect(renderedIds).toEqual([
      "message-human",
      "message-agent",
      "message-system",
      "message-stimulus",
      "message-buffered",
    ]);
    expect(screen.getByText("Ada Lovelace")).toBeVisible();
    expect(screen.getAllByText("Atlas").length).toBeGreaterThan(0);
    expect(screen.getByRole("note", { name: "System message" })).toHaveTextContent(
      "Atlas joined the room.",
    );
    expect(screen.getByLabelText("Stimulus message")).toHaveAttribute(
      "data-message-type",
      "stimulus",
    );
    expect(screen.getByLabelText("Protocol marker AGREE")).toBeVisible();
    expect(screen.getByLabelText("Protocol marker PASS")).toBeVisible();

    const unreadDivider = screen.getByRole("separator", {
      name: "Unread messages",
    });
    expect(unreadDivider.nextElementSibling).toHaveAttribute(
      "data-message-id",
      "message-agent",
    );
    fireEvent.click(screen.getAllByText("Inspect original message")[0]);
    expect(screen.getByText("[AGREE] Ship the bounded change.")).toBeVisible();
  });

  it("renders one snapshot message after its duplicate live create and applies later presence", async () => {
    const client = new FixtureHenosisClient();
    const openRoom = vi.spyOn(client, "openRoom");
    const observedEvents: RoomConversationEventEnvelope[] = [];
    const unlisten = await client.subscribeRoomEvents((event) =>
      observedEvents.push(event),
    );
    render(<RoomConversation client={client} roomId="room-orchard" />);

    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });
    const messageId = "room-orchard-message-5";
    const messageSelector = `[data-message-id="${messageId}"]`;
    await waitFor(() => {
      expect(timeline.querySelectorAll(messageSelector)).toHaveLength(1);
    });
    expect(openRoom).toHaveBeenCalledOnce();

    const openResult = openRoom.mock.results[0];
    if (!openResult || openResult.type !== "return") {
      throw new Error("The concrete fixture must return its opening snapshot.");
    }
    const openingSnapshot = await openResult.value;
    const duplicateMessage = openingSnapshot.page.messages.find(
      (candidate) => candidate.id === messageId,
    );
    if (!duplicateMessage) {
      throw new Error("The concrete fixture must expose the duplicate message.");
    }
    const streamId = openRoom.mock.calls[0]?.[1];
    if (!streamId) {
      throw new Error("RoomConversation must supply a fixture stream identifier.");
    }

    act(() => {
      client.emitRoomEvent("room-orchard", streamId, {
        type: "messageCreate",
        data: { roomId: "room-orchard", message: duplicateMessage },
      });
    });
    await waitFor(() => {
      expect(timeline.querySelectorAll(messageSelector)).toHaveLength(1);
    });

    act(() => {
      client.emitRoomEvent("room-orchard", streamId, {
        type: "presenceUpdate",
        data: {
          roomId: "room-orchard",
          userId: "fixture-agent",
          status: "online",
        },
      });
    });
    await waitFor(() => {
      const renderedMessage =
        timeline.querySelector<HTMLElement>(messageSelector);
      if (!renderedMessage) {
        throw new Error("The duplicate fixture message must remain rendered.");
      }
      expect(
        within(renderedMessage).getByLabelText("Mira is online"),
      ).toBeVisible();
    });
    expect(timeline.querySelectorAll(messageSelector)).toHaveLength(1);
    expect(
      observedEvents.map((event) => [event.sequence, event.event.type]),
    ).toEqual([
      [1, "messageCreate"],
      [2, "presenceUpdate"],
    ]);
    unlisten();
  });

  it("marks only a visibly reached live edge and offers a jump while away", async () => {
    const harness = createHarness();
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });
    const geometry = { height: 1_000, viewport: 200, top: 200 };
    installScrollGeometry(timeline, geometry);

    fireEvent.scroll(timeline);
    expect(
      await screen.findByRole("button", { name: "Jump to latest" }),
    ).toBeVisible();

    act(() => {
      harness.emit({
        type: "messageCreate",
        data: {
          roomId: ROOM_ID,
          message: message("message-live", "2026-08-01T12:05:00Z", {
            authorId: "agent-atlas",
            authorUsername: "atlas",
            authorDisplayName: "Atlas",
            content: "A live arrival.",
            messageType: "agent",
          }),
        },
      });
    });
    expect(harness.client.markRoomRead).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Jump to latest" }));
    expect(harness.client.markRoomRead).not.toHaveBeenCalled();
    expect(
      screen.getByRole("button", { name: "Jump to latest" }),
    ).toBeVisible();
    fireEvent.scroll(timeline);
    await waitFor(() => {
      expect(harness.client.markRoomRead).toHaveBeenCalledWith(
        ROOM_ID,
        expect.any(String),
        "message-live",
      );
    });
    expect(geometry.top).toBe(1_000);
    expect(
      screen.queryByRole("button", { name: "Jump to latest" }),
    ).not.toBeInTheDocument();
  });

  it("loads older history explicitly and preserves the visible scroll anchor", async () => {
    const harness = createHarness();
    let resolveOlder: ((result: RoomConversationCommandResult<MessagePage>) => void) | null =
      null;
    vi.mocked(harness.client.loadOlderMessages).mockImplementationOnce(
      async (_roomId, streamId) =>
        new Promise((resolve) => {
          resolveOlder = resolve;
          expect(streamId).toMatch(/^[A-Za-z0-9_-]{16,128}$/);
        }),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });
    const geometry = { height: 600, viewport: 240, top: 120 };
    installScrollGeometry(timeline, geometry);

    fireEvent.click(
      screen.getByRole("button", { name: "Load earlier messages" }),
    );
    expect(harness.client.loadOlderMessages).toHaveBeenCalledWith(
      ROOM_ID,
      expect.any(String),
      "message-human",
    );
    await act(async () => {
      resolveOlder?.({
        streamId: vi.mocked(harness.client.openRoom).mock.calls[0][1],
        sequence: 2,
        value: {
          messages: [
            message("message-older", "2026-08-01T11:59:00Z", {
              content: "An older anchored message.",
            }),
          ],
          hasOlder: false,
        },
      });
    });

    expect(screen.queryByText("An older anchored message.")).not.toBeInTheDocument();
    expect(geometry.top).toBe(120);
    geometry.height = 850;
    act(() => {
      harness.emit({
        type: "presenceUpdate",
        data: { roomId: ROOM_ID, userId: "agent-atlas", status: "online" },
      });
    });
    expect(await screen.findByText("An older anchored message.")).toBeVisible();
    expect(geometry.top).toBe(370);
    expect(
      screen.queryByRole("button", { name: "Load earlier messages" }),
    ).not.toBeInTheDocument();
  });

  it("throttles typing, keeps a failed draft, and retries an Enter send", async () => {
    const harness = createHarness();
    vi.mocked(harness.client.sendRoomMessage).mockRejectedValueOnce(
      new HenosisClientError("network", "Rift is temporarily unavailable."),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const composer = await screen.findByLabelText("Message Rift room");

    fireEvent.change(composer, { target: { value: "First draft" } });
    fireEvent.change(composer, { target: { value: "First draft revised" } });
    expect(harness.client.sendRoomTyping).toHaveBeenCalledOnce();
    fireEvent.keyDown(composer, { key: "Enter", shiftKey: true });
    expect(harness.client.sendRoomMessage).not.toHaveBeenCalled();

    fireEvent.keyDown(composer, { key: "Enter" });
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Rift is temporarily unavailable.",
    );
    expect(composer).toHaveValue("First draft revised");
    expect(composer).toHaveFocus();

    fireEvent.keyDown(composer, { key: "Enter" });
    await waitFor(() => expect(composer).toHaveValue(""));
    expect(harness.client.sendRoomMessage).toHaveBeenLastCalledWith(
      ROOM_ID,
      expect.any(String),
      "First draft revised",
      [],
    );
    expect(await screen.findByText("First draft revised")).toBeVisible();
  });

  it("preserves text entered while an earlier send is in flight", async () => {
    const harness = createHarness();
    let resolveSend:
      | ((result: RoomConversationCommandResult<RoomMessage | null>) => void)
      | null = null;
    vi.mocked(harness.client.sendRoomMessage).mockImplementationOnce(
      async () =>
        new Promise((resolve) => {
          resolveSend = resolve;
        }),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const composer = await screen.findByLabelText("Message Rift room");

    fireEvent.change(composer, { target: { value: "First message" } });
    fireEvent.keyDown(composer, { key: "Enter" });
    await waitFor(() => expect(harness.client.sendRoomMessage).toHaveBeenCalledOnce());
    fireEvent.change(composer, { target: { value: "Next message" } });
    const streamId = vi.mocked(harness.client.openRoom).mock.calls[0][1];
    await act(async () => {
      resolveSend?.({
        streamId,
        sequence: 1,
        value: message("message-slow-send", "2026-08-01T12:12:00Z", {
          content: "First message",
        }),
      });
    });

    expect(composer).toHaveValue("Next message");
  });

  it("stages path-free uploads, reports progress, removes them, and sends attachment-only", async () => {
    const harness = createHarness();
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    await screen.findByRole("log", { name: "Room message timeline" });

    fireEvent.click(screen.getByRole("button", { name: "Add attachments" }));
    expect(await screen.findByText("notes-1.txt")).toBeVisible();
    expect(document.body.innerHTML).not.toContain("NATIVE_PATH_SENTINEL");
    act(() => {
      harness.emit({
        type: "uploadProgress",
        data: {
          roomId: ROOM_ID,
          transferId: "upload-1",
          filename: "notes-1.txt",
          bytesSent: 64,
          totalBytes: 128,
        },
      });
    });
    expect(screen.getByLabelText("Upload progress for notes-1.txt")).toHaveValue(
      64,
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Remove notes-1.txt" }),
    );
    expect(screen.queryByText("notes-1.txt")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Add attachments" }));
    expect(await screen.findByText("notes-2.txt")).toBeVisible();
    fireEvent.keyDown(screen.getByLabelText("Message Rift room"), {
      key: "Enter",
    });
    await waitFor(() => {
      expect(harness.client.sendRoomMessage).toHaveBeenLastCalledWith(
        ROOM_ID,
        expect.any(String),
        "",
        ["upload-2"],
      );
    });
    expect(screen.queryByText("notes-2.txt")).not.toBeInTheDocument();
  });

  it("rejects a foreign send result without clearing its draft or attachments", async () => {
    const harness = createHarness();
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    await screen.findByRole("log", { name: "Room message timeline" });
    fireEvent.click(screen.getByRole("button", { name: "Add attachments" }));
    expect(await screen.findByText("notes-1.txt")).toBeVisible();
    vi.mocked(harness.client.sendRoomMessage).mockResolvedValueOnce({
      streamId: "foreign-room-stream",
      sequence: 2,
      value: message("message-foreign-send", "2026-08-01T12:13:00Z", {
        content: "This result belongs elsewhere.",
      }),
    });
    const composer = screen.getByLabelText("Message Rift room");

    fireEvent.change(composer, { target: { value: "Keep this draft" } });
    fireEvent.keyDown(composer, { key: "Enter" });

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Rift returned a room command that Henosis could not verify.",
    );
    expect(composer).toHaveValue("Keep this draft");
    expect(screen.getByText("notes-1.txt")).toBeVisible();
    expect(screen.queryByText("This result belongs elsewhere.")).not.toBeInTheDocument();
  });

  it("rejects foreign history and mutation results before local success effects", async () => {
    const harness = createHarness();
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });
    vi.mocked(harness.client.loadOlderMessages).mockResolvedValueOnce({
      streamId: "foreign-room-stream",
      sequence: 1,
      value: {
        messages: [
          message("message-foreign-history", "2026-08-01T11:58:00Z", {
            content: "Foreign history must stay hidden.",
          }),
        ],
        hasOlder: false,
      },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Load earlier messages" }),
    );
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Rift returned a room command that Henosis could not verify.",
    );
    expect(screen.queryByText("Foreign history must stay hidden.")).not.toBeInTheDocument();

    vi.mocked(harness.client.editRoomMessage).mockResolvedValueOnce({
      streamId: "foreign-room-stream",
      sequence: 1,
      value: message("message-human", "2026-08-01T12:00:00Z", {
        content: "Foreign edit must stay hidden.",
      }),
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Edit message from Ada Lovelace" }),
    );
    const editField = screen.getByRole("textbox", {
      name: "Edit message from Ada Lovelace",
    });
    fireEvent.change(editField, { target: { value: "Keep this local edit" } });
    fireEvent.click(screen.getByRole("button", { name: "Save edit" }));
    await waitFor(() => {
      expect(harness.client.editRoomMessage).toHaveBeenCalledOnce();
      expect(editField).toBeEnabled();
      expect(editField).toHaveFocus();
    });
    expect(editField).toHaveValue("Keep this local edit");

    fireEvent.click(screen.getByRole("button", { name: "Cancel edit" }));
    vi.mocked(harness.client.deleteRoomMessage).mockResolvedValueOnce({
      streamId: "foreign-room-stream",
      sequence: 1,
      value: "message-human",
    });
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Delete message from Ada Lovelace",
      }),
    );
    const confirmDelete = screen.getByRole("button", { name: "Confirm delete" });
    fireEvent.click(confirmDelete);
    await waitFor(() => {
      expect(harness.client.deleteRoomMessage).toHaveBeenCalledOnce();
      expect(confirmDelete).toBeEnabled();
      expect(confirmDelete).toHaveFocus();
    });
    expect(
      screen.getByRole("article", { name: "Message from Ada Lovelace" }),
    ).toBeVisible();
    fireEvent.click(screen.getByRole("button", { name: "Cancel deletion" }));

    vi.mocked(
      harness.client.selectAndUploadRoomAttachments,
    ).mockResolvedValueOnce({
      streamId: "foreign-room-stream",
      sequence: 1,
      value: [
        {
          uploadId: "foreign-upload",
          filename: "foreign-upload.txt",
          contentType: "text/plain",
          sizeBytes: 64,
        },
      ],
    });
    fireEvent.click(screen.getByRole("button", { name: "Add attachments" }));
    await waitFor(() => {
      expect(harness.client.selectAndUploadRoomAttachments).toHaveBeenCalledOnce();
    });
    expect(screen.queryByText("foreign-upload.txt")).not.toBeInTheDocument();
    expect(
      within(timeline).queryByText("Foreign edit must stay hidden."),
    ).not.toBeInTheDocument();
  });

  it("edits owned messages, confirms deletion, and restores useful focus", async () => {
    const harness = createHarness();
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });
    const humanMessage = screen.getByRole("article", {
      name: "Message from Ada Lovelace",
    });
    const agentMessage = screen.getByRole("article", {
      name: "Message from Atlas",
    });
    expect(
      within(agentMessage).queryByRole("button", {
        name: "Edit message from Atlas",
      }),
    ).not.toBeInTheDocument();
    expect(
      within(agentMessage).getByRole("button", {
        name: "Delete message from Atlas",
      }),
    ).toBeVisible();

    fireEvent.click(
      within(humanMessage).getByRole("button", {
        name: "Edit message from Ada Lovelace",
      }),
    );
    const editField = screen.getByLabelText("Edit message from Ada Lovelace");
    fireEvent.change(editField, { target: { value: "A corrected human thought." } });
    fireEvent.click(screen.getByRole("button", { name: "Save edit" }));
    await waitFor(() => {
      expect(
        screen.queryByRole("textbox", {
          name: "Edit message from Ada Lovelace",
        }),
      ).not.toBeInTheDocument();
      expect(
        screen.getByRole("article", {
          name: "Message from Ada Lovelace",
        }),
      ).toHaveTextContent("A corrected human thought.");
    });
    await waitFor(() => {
      expect(
        screen.getByRole("button", {
          name: "Edit message from Ada Lovelace",
        }),
      ).toHaveFocus();
    });

    const deleteButton = screen.getByRole("button", {
      name: "Delete message from Ada Lovelace",
    });
    fireEvent.click(deleteButton);
    expect(
      screen.getByRole("alertdialog", { name: "Delete message confirmation" }),
    ).toBeVisible();
    expect(harness.client.deleteRoomMessage).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel deletion" }));
    await waitFor(() =>
      expect(
        screen.getByRole("button", {
          name: "Delete message from Ada Lovelace",
        }),
      ).toHaveFocus(),
    );

    fireEvent.click(
      screen.getByRole("button", {
        name: "Delete message from Ada Lovelace",
      }),
    );
    fireEvent.click(screen.getByRole("button", { name: "Confirm delete" }));
    await waitFor(() => {
      expect(
        screen.queryByRole("article", {
          name: "Message from Ada Lovelace",
        }),
      ).not.toBeInTheDocument();
    });
    expect(timeline).toHaveFocus();
  });

  it("restores edit and delete focus only after failed controls are enabled", async () => {
    const harness = createHarness();
    let rejectEdit: ((reason?: unknown) => void) | null = null;
    let rejectDelete: ((reason?: unknown) => void) | null = null;
    vi.mocked(harness.client.editRoomMessage).mockImplementationOnce(
      async () =>
        new Promise((_, reject) => {
          rejectEdit = reject;
        }),
    );
    vi.mocked(harness.client.deleteRoomMessage).mockImplementationOnce(
      async () =>
        new Promise((_, reject) => {
          rejectDelete = reject;
        }),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const timeline = await screen.findByRole("log", {
      name: "Room message timeline",
    });

    fireEvent.click(
      screen.getByRole("button", { name: "Edit message from Ada Lovelace" }),
    );
    const editField = screen.getByRole("textbox", {
      name: "Edit message from Ada Lovelace",
    });
    fireEvent.change(editField, { target: { value: "A failed correction." } });
    fireEvent.click(screen.getByRole("button", { name: "Save edit" }));
    await waitFor(() => expect(editField).toBeDisabled());
    act(() => timeline.focus());
    await act(async () => {
      rejectEdit?.(new HenosisClientError("network", "Edit failed safely."));
    });
    await waitFor(() => {
      expect(editField).toBeEnabled();
      expect(editField).toHaveFocus();
    });

    fireEvent.click(screen.getByRole("button", { name: "Cancel edit" }));
    fireEvent.click(
      await screen.findByRole("button", {
        name: "Delete message from Ada Lovelace",
      }),
    );
    const confirmDelete = screen.getByRole("button", { name: "Confirm delete" });
    fireEvent.click(confirmDelete);
    await waitFor(() => expect(confirmDelete).toBeDisabled());
    act(() => timeline.focus());
    await act(async () => {
      rejectDelete?.(new HenosisClientError("network", "Delete failed safely."));
    });
    await waitFor(() => {
      expect(confirmDelete).toBeEnabled();
      expect(confirmDelete).toHaveFocus();
    });
  });

  it("hides moderation actions without the matching server capability", async () => {
    const harness = createHarness();
    vi.mocked(harness.client.openRoom).mockImplementationOnce(
      async (_roomId, streamId) =>
        snapshot(streamId, {
          permissions: {
            sendMessages: false,
            attachFiles: false,
            manageMessages: false,
            manageServer: false,
          },
        }),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);
    const agentMessage = await screen.findByRole("article", {
      name: "Message from Atlas",
    });

    expect(
      within(agentMessage).queryByRole("button", {
        name: "Delete message from Atlas",
      }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Send message" })).toBeDisabled();
    expect(
      screen.getByRole("button", { name: "Add attachments" }),
    ).toBeDisabled();
  });

  it("announces typing, presence, reconnect state, expiry, and exact cleanup", async () => {
    const harness = createHarness();
    const rendered = render(
      <RoomConversation client={harness.client} roomId={ROOM_ID} />,
    );
    await screen.findByRole("log", { name: "Room message timeline" });
    vi.useFakeTimers();

    act(() => {
      harness.emit({
        type: "presenceUpdate",
        data: { roomId: ROOM_ID, userId: "agent-atlas", status: "online" },
      });
      harness.emit({
        type: "typingStart",
        data: { roomId: ROOM_ID, userId: "agent-atlas", username: "Atlas" },
      });
      harness.emit({
        type: "connectionChanged",
        data: { roomId: ROOM_ID, status: "reconnecting" },
      });
    });
    expect(screen.getByLabelText("Atlas is online")).toBeVisible();
    expect(screen.getByText("Atlas is typing…")).toBeVisible();
    expect(screen.getByRole("status", { name: "Connection status" })).toHaveTextContent(
      "Reconnecting",
    );

    act(() => vi.advanceTimersByTime(TYPING_EXPIRY_MS));
    expect(screen.queryByText("Atlas is typing…")).not.toBeInTheDocument();
    act(() => {
      harness.emit({
        type: "connectionChanged",
        data: { roomId: ROOM_ID, status: "connected" },
      });
    });
    expect(screen.getByRole("status", { name: "Connection status" })).toHaveTextContent(
      "Connected",
    );

    const streamId = vi.mocked(harness.client.openRoom).mock.calls[0][1];
    rendered.unmount();
    expect(harness.unlisten).toHaveBeenCalledOnce();
    expect(harness.client.closeRoom).toHaveBeenCalledWith(ROOM_ID, streamId);
  });

  it("immediately releases a generation whose opening snapshot does not match", async () => {
    const harness = createHarness();
    vi.mocked(harness.client.openRoom).mockImplementationOnce(
      async (_roomId, streamId) => snapshot(`${streamId}-mismatch`),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Rift returned a room generation that Henosis could not verify.",
    );
    const attemptedStreamId = vi.mocked(harness.client.openRoom).mock.calls[0][1];
    expect(harness.unlisten).toHaveBeenCalledOnce();
    expect(harness.client.closeRoom).toHaveBeenCalledWith(
      ROOM_ID,
      attemptedStreamId,
    );
  });

  it("retries a safe opening error with a fresh one-use stream", async () => {
    const harness = createHarness();
    vi.mocked(harness.client.openRoom).mockRejectedValueOnce(
      new HenosisClientError("network", "Rift could not open this room."),
    );
    render(<RoomConversation client={harness.client} roomId={ROOM_ID} />);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Rift could not open this room.",
    );
    fireEvent.click(
      screen.getByRole("button", { name: "Retry room conversation" }),
    );
    await screen.findByRole("log", { name: "Room message timeline" });

    const streamIds = vi
      .mocked(harness.client.openRoom)
      .mock.calls.map((call) => call[1]);
    expect(streamIds).toHaveLength(2);
    expect(streamIds[0]).toMatch(/^[A-Za-z0-9_-]{16,128}$/);
    expect(streamIds[1]).toMatch(/^[A-Za-z0-9_-]{16,128}$/);
    expect(streamIds[1]).not.toBe(streamIds[0]);
    expect(harness.unlisten).toHaveBeenCalledOnce();
    expect(harness.client.closeRoom).toHaveBeenCalledWith(ROOM_ID, streamIds[0]);
  });
});
