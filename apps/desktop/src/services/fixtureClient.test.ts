/** Contract tests for the stateful, network-free room conversation fixture. */
import { describe, expect, it, vi } from "vitest";
import type { RoomConversationEventEnvelope } from "../domain/conversation";
import { FixtureHenosisClient } from "./fixtureClient";
import { HenosisClientError } from "./henosisClient";

/** Build one production-valid one-use room stream identifier for fixture tests. */
function fixtureStream(suffix: string): string {
  return `fixture-stream-${suffix.padEnd(16, "0")}`;
}

describe("FixtureHenosisClient", () => {
  it("labels its directory source and exposes no token-shaped fields", async () => {
    const client = new FixtureHenosisClient();

    const result = await client.bootstrap();
    const serialized = JSON.stringify(result);

    expect(result.directory?.source).toBe("fixture");
    expect(result.directory?.rooms.length).toBeGreaterThanOrEqual(3);
    expect(serialized).not.toMatch(/access.?token|refresh.?token/i);
  });

  it("opens an oldest-first live window and paginates only from its oldest cursor", async () => {
    const client = new FixtureHenosisClient();
    const streamId = fixtureStream("pagination");
    const snapshot = await client.openRoom("room-orchard", streamId);

    expect(snapshot).toMatchObject({
      roomId: "room-orchard",
      streamId,
      lastEventSequence: 0,
      currentUserId: "fixture-user",
      connectionStatus: "connected",
      page: { hasOlder: true },
    });
    expect(snapshot.page.messages.map((message) => message.id)).toEqual([
      "room-orchard-message-3",
      "room-orchard-message-4",
      "room-orchard-message-5",
    ]);

    await expect(
      client.loadOlderMessages("room-orchard", streamId, "wrong-cursor"),
    ).rejects.toMatchObject({ kind: "validation" });

    const page = await client.loadOlderMessages(
      "room-orchard",
      streamId,
      snapshot.page.messages[0].id,
    );
    expect(page).toEqual({
      streamId,
      sequence: 1,
      value: {
        messages: expect.arrayContaining([
          expect.objectContaining({ id: "room-orchard-message-1" }),
          expect.objectContaining({ id: "room-orchard-message-2" }),
        ]),
        hasOlder: false,
      },
    });
    expect(page.value.messages.map((message) => message.id)).toEqual([
      "room-orchard-message-1",
      "room-orchard-message-2",
    ]);
  });

  it("orders create, edit, and delete results with idempotent fixture events", async () => {
    const client = new FixtureHenosisClient();
    const streamId = fixtureStream("mutations");
    const events: RoomConversationEventEnvelope[] = [];
    const unlisten = await client.subscribeRoomEvents((event) => events.push(event));
    await client.openRoom("room-orchard", streamId);

    const created = await client.sendRoomMessage(
      "room-orchard",
      streamId,
      "A fixture message",
      [],
    );
    expect(created.sequence).toBe(1);
    expect(created.value).toMatchObject({
      roomId: "room-orchard",
      authorId: "fixture-user",
      content: "A fixture message",
    });

    const messageId = created.value?.id ?? "missing-message";
    const edited = await client.editRoomMessage(
      "room-orchard",
      streamId,
      messageId,
      "An edited fixture message",
    );
    const deleted = await client.deleteRoomMessage(
      "room-orchard",
      streamId,
      messageId,
    );

    expect(edited).toMatchObject({
      sequence: 3,
      value: { id: messageId, content: "An edited fixture message" },
    });
    expect(deleted).toEqual({
      streamId,
      sequence: 5,
      value: messageId,
    });
    expect(events.map((event) => [event.sequence, event.event.type])).toEqual([
      [2, "messageCreate"],
      [4, "messageUpdate"],
      [6, "messageDelete"],
    ]);

    unlisten();
    unlisten();
    await client.sendRoomMessage("room-orchard", streamId, "After cleanup", []);
    expect(events).toHaveLength(3);
  });

  it("stages path-free attachments and emits bounded upload progress", async () => {
    const client = new FixtureHenosisClient();
    const streamId = fixtureStream("uploads");
    const listener = vi.fn();
    await client.subscribeRoomEvents(listener);
    await client.openRoom("room-orchard", streamId);

    const uploaded = await client.selectAndUploadRoomAttachments(
      "room-orchard",
      streamId,
    );
    const serialized = JSON.stringify({ uploaded, events: listener.mock.calls });

    expect(uploaded).toEqual({
      streamId,
      sequence: 3,
      value: [
        {
          uploadId: "fixture-upload-1",
          filename: "fixture-note.txt",
          contentType: "text/plain",
          sizeBytes: 128,
        },
      ],
    });
    expect(listener.mock.calls.map(([event]) => event.sequence)).toEqual([1, 2]);
    expect(listener.mock.calls[1]?.[0]).toMatchObject({
      event: {
        type: "uploadProgress",
        data: { bytesSent: 128, totalBytes: 128 },
      },
    });
    expect(serialized).not.toMatch(/(?:^|["'])path["']|\/home\/|[A-Za-z]:\\\\/i);

    const sent = await client.sendRoomMessage(
      "room-orchard",
      streamId,
      "",
      [uploaded.value[0].uploadId],
    );
    expect(sent.value?.attachments[0]).toMatchObject({
      filename: "fixture-note.txt",
      contentType: "text/plain",
      sizeBytes: 128,
    });
  });

  it("keeps replacement generations isolated and persists a monotonic read marker", async () => {
    const client = new FixtureHenosisClient();
    const oldStreamId = fixtureStream("old");
    const currentStreamId = fixtureStream("current");
    const nextStreamId = fixtureStream("next");
    const first = await client.openRoom("room-orchard", oldStreamId);
    const latestMessageId = first.page.messages.at(-1)?.id ?? "missing-message";
    await client.markRoomRead("room-orchard", oldStreamId, latestMessageId);
    const staged = await client.selectAndUploadRoomAttachments(
      "room-orchard",
      oldStreamId,
    );
    await client.openRoom("room-orchard", currentStreamId);

    await expect(
      client.closeRoom("room-orchard", oldStreamId),
    ).rejects.toBeInstanceOf(HenosisClientError);
    await expect(
      client.sendRoomMessage("room-orchard", oldStreamId, "stale", []),
    ).rejects.toMatchObject({ kind: "validation" });
    await expect(
      client.sendRoomMessage(
        "room-orchard",
        currentStreamId,
        "",
        [staged.value[0].uploadId],
      ),
    ).rejects.toMatchObject({ kind: "validation" });

    const current = await client.openRoom("room-orchard", nextStreamId);
    expect(current.unreadBoundary).toEqual({ kind: "none" });
    await expect(
      client.sendRoomTyping("room-orchard", nextStreamId),
    ).resolves.toBeUndefined();
    await expect(
      client.closeRoom("room-orchard", nextStreamId),
    ).resolves.toBeUndefined();
  });

  it("requires production-shaped one-use stream capabilities", async () => {
    const client = new FixtureHenosisClient();
    const firstStreamId = fixtureStream("capability-first");
    const currentStreamId = fixtureStream("capability-current");

    await expect(
      client.openRoom("room-orchard", "too-short"),
    ).rejects.toMatchObject({ kind: "validation" });
    await client.openRoom("room-orchard", firstStreamId);
    await client.openRoom("room-orchard", currentStreamId);
    await expect(
      client.openRoom("room-orchard", firstStreamId),
    ).rejects.toMatchObject({ kind: "validation" });
    await expect(
      client.sendRoomTyping("room-orchard", currentStreamId),
    ).resolves.toBeUndefined();
  });

  it("does not advance the read marker when the current user sends", async () => {
    const client = new FixtureHenosisClient();
    const firstStreamId = fixtureStream("read-before");
    const nextStreamId = fixtureStream("read-after");
    const first = await client.openRoom("room-orchard", firstStreamId);

    await client.sendRoomMessage(
      "room-orchard",
      firstStreamId,
      "A message does not prove visibility.",
      [],
    );
    const next = await client.openRoom("room-orchard", nextStreamId);

    expect(first.unreadBoundary).toEqual({
      kind: "beforeMessage",
      messageId: "room-orchard-message-3",
    });
    expect(next.unreadBoundary).toEqual({ kind: "beforeLoadedWindow" });
  });

  it("mirrors native attachment and edit validation", async () => {
    const client = new FixtureHenosisClient();
    const streamId = fixtureStream("validation");
    const snapshot = await client.openRoom("room-orchard", streamId);
    const uploaded = await client.selectAndUploadRoomAttachments(
      "room-orchard",
      streamId,
    );
    const uploadId = uploaded.value[0].uploadId;
    const messageId = snapshot.page.messages.at(-1)?.id ?? "missing-message";

    await expect(
      client.sendRoomMessage("room-orchard", streamId, "", [uploadId, uploadId]),
    ).rejects.toMatchObject({ kind: "validation" });
    await expect(
      client.sendRoomMessage(
        "room-orchard",
        streamId,
        "too many",
        Array.from({ length: 11 }, (_, index) => `upload-${index}`),
      ),
    ).rejects.toMatchObject({ kind: "validation" });
    await expect(
      client.editRoomMessage("room-orchard", streamId, messageId, "   "),
    ).rejects.toMatchObject({ kind: "validation" });
  });
});
