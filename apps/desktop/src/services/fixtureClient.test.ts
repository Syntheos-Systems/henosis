/** Contract tests for the stateful, network-free room conversation fixture. */
import { describe, expect, it, vi } from "vitest";
import type { RoomConversationEventEnvelope } from "../domain/conversation";
import type { ApplyAgentRosterRequest } from "../domain/agentControl";
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

  it("exposes dynamic capabilities and owned, foreign-owned, and unowned roster state", async () => {
    const client = new FixtureHenosisClient();

    const agents = await client.getMyAgents();
    const catalog = await client.getAgentCapabilities("server-henosis");
    const permissions = await client.getRoomPermissions("server-henosis");
    const roster = await client.getRoomAgentRoster("server-henosis");
    const serialized = JSON.stringify({ agents, catalog, permissions, roster });

    expect(agents).toEqual([
      expect.objectContaining({
        id: "agent-mira",
        ownerUserId: "fixture-user",
      }),
      expect.objectContaining({
        id: "agent-lumen",
        ownerUserId: "fixture-user",
      }),
    ]);
    expect(catalog.harnesses).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          id: "codex-cli",
          models: expect.arrayContaining([
            expect.objectContaining({ id: "gpt-5.6-sol" }),
          ]),
        }),
        expect.objectContaining({
          id: "claude-code",
          models: expect.arrayContaining([
            expect.objectContaining({ id: "claude-opus" }),
            expect.objectContaining({ id: "claude-sonnet" }),
          ]),
        }),
      ]),
    );
    expect(permissions).toEqual({
      sendMessages: true,
      attachFiles: true,
      manageMessages: true,
      manageServer: true,
    });
    expect(roster.seats).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          agentIdentityId: "agent-mira",
          agentUsername: "mira",
          agentDisplayName: "Mira",
          ownerHumanId: "fixture-user",
          credentialReadiness: "hostSession",
        }),
        expect.objectContaining({
          agentIdentityId: "agent-cinder",
          agentUsername: "cinder",
          agentDisplayName: "Cinder",
          ownerHumanId: "human-steward",
          credentialReadiness: "ready",
        }),
        expect.objectContaining({
          agentIdentityId: "agent-imported",
          agentUsername: "imported-scout",
          agentDisplayName: "Imported scout",
          ownerHumanId: null,
          credentialReadiness: "attention",
        }),
      ]),
    );
    expect(serialized).not.toMatch(
      /access.?token|refresh.?token|api.?key|private.?key|executable.?path|\/home\/|[A-Za-z]:\\\\/i,
    );
  });

  it("returns detached dashboard data that cannot mutate fixture authority", async () => {
    const client = new FixtureHenosisClient();
    const agents = await client.getMyAgents();
    const catalog = await client.getAgentCapabilities("server-henosis");
    const roster = await client.getRoomAgentRoster("server-henosis");

    (agents[0] as { displayName: string | null }).displayName = "Mutated";
    (catalog.harnesses[0] as { label: string }).label = "Mutated";
    (roster.seats[0].settings as Record<string, unknown>).effort = "mutated";

    await expect(client.getMyAgents()).resolves.toEqual(
      expect.arrayContaining([
        expect.objectContaining({ id: "agent-mira", displayName: "Mira" }),
      ]),
    );
    await expect(client.getAgentCapabilities("server-henosis")).resolves.toEqual(
      expect.objectContaining({
        harnesses: expect.arrayContaining([
          expect.objectContaining({ id: "codex-cli", label: "Codex CLI" }),
        ]),
      }),
    );
    await expect(client.getRoomAgentRoster("server-henosis")).resolves.toEqual(
      expect.objectContaining({
        seats: expect.arrayContaining([
          expect.objectContaining({
            seatId: "seat-mira",
            settings: expect.objectContaining({ effort: "medium" }),
          }),
        ]),
      }),
    );
  });

  it("creates owned identities and claims a known imported roster identity", async () => {
    const client = new FixtureHenosisClient();

    const created = await client.createMyAgent("builder", "Builder");
    const claimed = await client.claimAgent("agent-imported");
    const agents = await client.getMyAgents();
    const roster = await client.getRoomAgentRoster("server-henosis");

    expect(created).toMatchObject({
      id: "fixture-agent-created-1",
      username: "builder",
      displayName: "Builder",
      ownerUserId: "fixture-user",
    });
    expect(claimed).toMatchObject({
      id: "agent-imported",
      ownerUserId: "fixture-user",
    });
    expect(agents.map((agent) => agent.id)).toEqual([
      "agent-mira",
      "agent-lumen",
      "fixture-agent-created-1",
      "agent-imported",
    ]);
    expect(
      roster.seats.find((seat) => seat.agentIdentityId === "agent-imported"),
    ).toMatchObject({
      agentUsername: "imported-scout",
      agentDisplayName: "Imported scout",
      ownerHumanId: "fixture-user",
    });
    await expect(client.createMyAgent("builder", null)).rejects.toMatchObject({
      kind: "conflict",
      code: "agent_username_taken",
    });
  });

  it("advances desired revision atomically and rejects a stale replacement", async () => {
    const client = new FixtureHenosisClient();
    const initial = await client.getRoomAgentRoster("server-henosis");
    const update: ApplyAgentRosterRequest = {
      expectedRevision: initial.desiredRevision,
      seats: initial.seats.map((seat) => ({
        seatId: seat.seatId,
        agentIdentityId: seat.agentIdentityId,
        harnessKey: seat.harnessKey,
        modelKey: seat.modelKey,
        settings: seat.settings,
        credentialBindingId: seat.credentialBindingId,
        enabled: seat.enabled,
        position: seat.position,
      })),
    };

    const applied = await client.applyRoomAgentRoster("server-henosis", update);

    expect(applied).toMatchObject({
      desiredRevision: 3,
      activeRevision: 2,
      lastGoodRevision: 2,
      runtimeActivation: "pending",
      runtimeErrorCode: null,
    });
    expect(applied.seats.every((seat) => seat.configurationRevision === 3)).toBe(
      true,
    );
    expect(applied.seats[0]).toMatchObject({
      agentUsername: "mira",
      agentDisplayName: "Mira",
    });
    await expect(
      client.applyRoomAgentRoster("server-henosis", update),
    ).rejects.toMatchObject({
      kind: "conflict",
      code: "revision_conflict",
    });
  });

  it("rejects an unknown roster identity without mutating retained state", async () => {
    const client = new FixtureHenosisClient();
    const initial = await client.getRoomAgentRoster("server-henosis");
    const unknownIdentityUpdate: ApplyAgentRosterRequest = {
      expectedRevision: initial.desiredRevision,
      seats: initial.seats.map((seat, index) => ({
        seatId: seat.seatId,
        agentIdentityId: index === 0 ? "agent-unknown" : seat.agentIdentityId,
        harnessKey: seat.harnessKey,
        modelKey: seat.modelKey,
        settings: seat.settings,
        credentialBindingId: seat.credentialBindingId,
        enabled: seat.enabled,
        position: seat.position,
      })),
    };

    await expect(
      client.applyRoomAgentRoster("server-henosis", unknownIdentityUpdate),
    ).rejects.toMatchObject({
      kind: "validation",
      code: "fixture_agent_not_found",
    });
    await expect(client.getRoomAgentRoster("server-henosis")).resolves.toEqual(
      initial,
    );
  });

  it("transitions pending activation to active or injected failure without losing last-good", async () => {
    const successfulClient = new FixtureHenosisClient();
    const successfulInitial = await successfulClient.getRoomAgentRoster(
      "server-henosis",
    );
    const successfulUpdate: ApplyAgentRosterRequest = {
      expectedRevision: successfulInitial.desiredRevision,
      seats: successfulInitial.seats.map((seat) => ({
        seatId: seat.seatId,
        agentIdentityId: seat.agentIdentityId,
        harnessKey: seat.harnessKey,
        modelKey: seat.modelKey,
        settings: seat.settings,
        credentialBindingId: seat.credentialBindingId,
        enabled: seat.enabled,
        position: seat.position,
      })),
    };
    const pending = await successfulClient.applyRoomAgentRoster(
      "server-henosis",
      successfulUpdate,
    );
    const active = await successfulClient.getRoomBridgeStatus("server-henosis");

    expect(pending.runtimeActivation).toBe("pending");
    expect(active).toMatchObject({
      desiredRevision: 3,
      activeRevision: 3,
      lastGoodRevision: 3,
      runtimeActivation: "active",
    });

    const failedClient = new FixtureHenosisClient();
    failedClient.injectNextAgentActivationFailure(
      "server-henosis",
      "fixture_start_failed",
    );
    const failedInitial = await failedClient.getRoomAgentRoster("server-henosis");
    const failedUpdate: ApplyAgentRosterRequest = {
      expectedRevision: failedInitial.desiredRevision,
      seats: failedInitial.seats.map((seat) => ({
        seatId: seat.seatId,
        agentIdentityId: seat.agentIdentityId,
        harnessKey: seat.harnessKey,
        modelKey: seat.modelKey,
        settings: seat.settings,
        credentialBindingId: seat.credentialBindingId,
        enabled: seat.enabled,
        position: seat.position,
      })),
    };
    await failedClient.applyRoomAgentRoster("server-henosis", failedUpdate);
    const failed = await failedClient.getRoomBridgeStatus("server-henosis");
    const retrying = await failedClient.reconcileRoomBridge("server-henosis");
    const recovered = await failedClient.getRoomBridgeStatus("server-henosis");

    expect(failed).toMatchObject({
      desiredRevision: 3,
      activeRevision: 2,
      lastGoodRevision: 2,
      runtimeActivation: "failed",
      runtimeErrorCode: "fixture_start_failed",
    });
    expect(retrying).toMatchObject({
      desiredRevision: 3,
      activeRevision: 2,
      lastGoodRevision: 2,
      runtimeActivation: "pending",
    });
    expect(recovered).toMatchObject({
      desiredRevision: 3,
      activeRevision: 3,
      lastGoodRevision: 3,
      runtimeActivation: "active",
    });
  });

  it("pauses and resumes one known fixture bridge without changing revisions", async () => {
    const client = new FixtureHenosisClient();

    const paused = await client.pauseRoomBridge("server-henosis");
    const resumed = await client.resumeRoomBridge("server-henosis");

    expect(paused).toMatchObject({
      paused: true,
      desiredRevision: 2,
      activeRevision: 2,
    });
    expect(resumed).toMatchObject({
      paused: false,
      desiredRevision: 2,
      activeRevision: 2,
    });
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

  it("publishes explicit events only through the exact active generation", async () => {
    const client = new FixtureHenosisClient();
    const firstStreamId = fixtureStream("explicit-events-first");
    const nextStreamId = fixtureStream("explicit-events-next");
    const events: RoomConversationEventEnvelope[] = [];
    await client.subscribeRoomEvents((event) => events.push(event));
    const snapshot = await client.openRoom("room-orchard", firstStreamId);
    const duplicateMessage = snapshot.page.messages.at(-1);
    if (!duplicateMessage) {
      throw new Error("The fixture snapshot must include its newest message.");
    }

    client.emitRoomEvent("room-orchard", firstStreamId, {
      type: "messageCreate",
      data: { roomId: "room-orchard", message: duplicateMessage },
    });
    expect(() =>
      client.emitRoomEvent("room-orchard", firstStreamId, {
        type: "presenceUpdate",
        data: {
          roomId: "room-workshop",
          userId: "fixture-agent",
          status: "online",
        },
      }),
    ).toThrow("A fixture room event must target its active room.");
    client.emitRoomEvent("room-orchard", firstStreamId, {
      type: "presenceUpdate",
      data: {
        roomId: "room-orchard",
        userId: "fixture-agent",
        status: "online",
      },
    });

    expect(events.map((event) => [event.sequence, event.event.type])).toEqual([
      [1, "messageCreate"],
      [2, "presenceUpdate"],
    ]);

    await client.openRoom("room-orchard", nextStreamId);
    expect(() =>
      client.emitRoomEvent("room-orchard", firstStreamId, {
        type: "presenceUpdate",
        data: {
          roomId: "room-orchard",
          userId: "fixture-agent",
          status: "idle",
        },
      }),
    ).toThrow("That fixture room generation is no longer active.");
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
