/** Interaction tests for the accessible room-dashboard shell. */
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type {
  AgentCapabilityCatalog,
  AgentRosterSnapshot,
  OwnedAgentIdentity,
  RoomBridgeStatus,
} from "../domain/agentControl";
import type { RoomPermissions } from "../domain/conversation";
import type { RoomSummary } from "../domain/rooms";
import { createFixtureRooms } from "../data/fixtureRooms";
import { FixtureHenosisClient } from "../services/fixtureClient";
import { HenosisClientError } from "../services/henosisClient";
import { DashboardTabs } from "./DashboardTabs";
import { RoomDashboard } from "./RoomDashboard";
import {
  confirmDashboardNavigation,
  RoomDetail,
} from "./RoomDetail";

/** One externally resolvable promise used to prove requests start concurrently. */
interface Deferred<T> {
  /** Promise observed by the component. */
  readonly promise: Promise<T>;
  /** Resolve the observed promise. */
  readonly resolve: (value: T) => void;
}

/** Controls the matchMedia result and emits deterministic breakpoint changes. */
interface ViewportController {
  /** Move to one viewport width and notify registered media-query listeners. */
  readonly setWidth: (width: number) => void;
}

/** Create one externally resolvable promise. */
function deferred<T>(): Deferred<T> {
  let resolvePromise: ((value: T) => void) | undefined;
  const promise = new Promise<T>((resolve) => {
    resolvePromise = resolve;
  });
  return {
    promise,
    resolve(value: T) {
      resolvePromise?.(value);
    },
  };
}

/** Return the primary fixture room with a compile-time absence guard. */
function primaryRoom(): RoomSummary {
  const room = createFixtureRooms(new Date("2026-08-01T12:00:00.000Z"))[0];
  if (!room) {
    throw new Error("The dashboard test requires the primary fixture room.");
  }
  return room;
}

/** Install deterministic matchMedia behavior for one viewport width. */
function installViewport(width: number): ViewportController {
  let currentWidth = width;
  const listeners = new Set<() => void>();
  const mediaQueryList = {
    get matches() {
      return currentWidth >= 1180;
    },
    media: "(min-width: 1180px)",
    onchange: null,
    addEventListener: vi.fn((_type: string, listener: () => void) => {
      listeners.add(listener);
    }),
    removeEventListener: vi.fn((_type: string, listener: () => void) => {
      listeners.delete(listener);
    }),
    addListener: vi.fn((listener: () => void) => listeners.add(listener)),
    removeListener: vi.fn((listener: () => void) => listeners.delete(listener)),
    dispatchEvent: vi.fn(() => true),
  };
  vi.stubGlobal(
    "matchMedia",
    vi.fn(() => mediaQueryList),
  );
  return {
    setWidth(nextWidth: number) {
      currentWidth = nextWidth;
      listeners.forEach((listener) => listener());
    },
  };
}

/** Render a connected fixture dashboard and wait for its default tab. */
async function renderDashboard(
  client: FixtureHenosisClient = new FixtureHenosisClient(),
  room: RoomSummary = primaryRoom(),
): Promise<FixtureHenosisClient> {
  render(
    <RoomDashboard
      client={client}
      room={room}
      currentUserId="fixture-user"
      onReconnect={vi.fn()}
    />,
  );
  await screen.findByRole("tab", { name: "Agents" });
  return client;
}

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("RoomDashboard", () => {
  it("defaults to Agents and implements automatic WAI-ARIA tab navigation", async () => {
    await renderDashboard();

    const tablist = screen.getByRole("tablist", { name: "Room controls" });
    const agents = within(tablist).getByRole("tab", { name: "Agents" });
    const people = within(tablist).getByRole("tab", { name: "People" });
    const room = within(tablist).getByRole("tab", { name: "Room" });
    expect(agents).toHaveAttribute("aria-selected", "true");
    expect(agents).toHaveAttribute("aria-controls", "dashboard-panel-agents");
    expect(screen.getByRole("tabpanel", { name: "Agents" })).toHaveAttribute(
      "aria-labelledby",
      agents.id,
    );

    agents.focus();
    fireEvent.keyDown(agents, { key: "ArrowRight" });
    expect(people).toHaveFocus();
    expect(people).toHaveAttribute("aria-selected", "true");
    expect(screen.getByRole("tabpanel", { name: "People" })).toBeInTheDocument();

    fireEvent.keyDown(people, { key: "ArrowLeft" });
    expect(agents).toHaveFocus();
    fireEvent.keyDown(agents, { key: "End" });
    expect(room).toHaveFocus();
    expect(room).toHaveAttribute("aria-selected", "true");
    fireEvent.keyDown(room, { key: "ArrowRight" });
    expect(agents).toHaveFocus();
    fireEvent.keyDown(agents, { key: "ArrowLeft" });
    expect(room).toHaveFocus();
    fireEvent.keyDown(room, { key: "Home" });
    expect(agents).toHaveFocus();
  });

  it("starts every sanitized read-context request before awaiting any result", async () => {
    const client = new FixtureHenosisClient();
    const [owned, catalog, permissions, roster, bridge] = await Promise.all([
      client.getMyAgents(),
      client.getAgentCapabilities("server-henosis"),
      client.getRoomPermissions("server-henosis"),
      client.getRoomAgentRoster("server-henosis"),
      client.getRoomBridgeStatus("server-henosis"),
    ]);
    const ownedResult = deferred<OwnedAgentIdentity[]>();
    const catalogResult = deferred<AgentCapabilityCatalog>();
    const permissionResult = deferred<RoomPermissions>();
    const rosterResult = deferred<AgentRosterSnapshot>();
    const bridgeResult = deferred<RoomBridgeStatus>();
    const ownedSpy = vi
      .spyOn(client, "getMyAgents")
      .mockImplementation(() => ownedResult.promise);
    const catalogSpy = vi
      .spyOn(client, "getAgentCapabilities")
      .mockImplementation(() => catalogResult.promise);
    const permissionSpy = vi
      .spyOn(client, "getRoomPermissions")
      .mockImplementation(() => permissionResult.promise);
    const rosterSpy = vi
      .spyOn(client, "getRoomAgentRoster")
      .mockImplementation(() => rosterResult.promise);
    const bridgeSpy = vi
      .spyOn(client, "getRoomBridgeStatus")
      .mockImplementation(() => bridgeResult.promise);

    render(
      <RoomDashboard
        client={client}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );

    await waitFor(() => {
      expect(ownedSpy).toHaveBeenCalledOnce();
      expect(catalogSpy).toHaveBeenCalledWith("server-henosis");
      expect(permissionSpy).toHaveBeenCalledWith("server-henosis");
      expect(rosterSpy).toHaveBeenCalledWith("server-henosis");
      expect(bridgeSpy).toHaveBeenCalledWith("server-henosis");
    });
    ownedResult.resolve(owned);
    catalogResult.resolve(catalog);
    permissionResult.resolve(permissions);
    rosterResult.resolve(roster);
    bridgeResult.resolve(bridge);

    expect(await screen.findByText("4 identities available")).toBeInTheDocument();
    expect(screen.getByText("3 agents in this room")).toBeInTheDocument();
    expect(screen.getByText("Room manager")).toBeInTheDocument();
    expect(screen.getByText("Bridge active")).toBeInTheDocument();
  });

  it("renders an honest empty state without inferring authority from owned agents", async () => {
    const room = createFixtureRooms().find(
      (candidate) => candidate.serverId === "server-trust",
    );
    if (!room) {
      throw new Error("The dashboard test requires the Trust Lab fixture room.");
    }

    await renderDashboard(new FixtureHenosisClient(), room);

    expect(
      await screen.findByText("No agents in this room yet."),
    ).toBeInTheDocument();
    expect(screen.getByText("Member access")).toBeInTheDocument();
  });

  it("updates bridge state from authoritative manager pause and resume results", async () => {
    const client = new FixtureHenosisClient();
    const pauseSpy = vi.spyOn(client, "pauseRoomBridge");
    const resumeSpy = vi.spyOn(client, "resumeRoomBridge");
    await renderDashboard(client);

    fireEvent.click(screen.getByRole("tab", { name: "Room" }));
    fireEvent.click(screen.getByRole("button", { name: "Pause bridge" }));

    expect(
      await screen.findByRole("heading", { name: "Bridge paused" }),
    ).toBeInTheDocument();
    expect(pauseSpy).toHaveBeenCalledWith("server-henosis");
    fireEvent.click(screen.getByRole("button", { name: "Resume bridge" }));

    expect(
      await screen.findByRole("heading", { name: "Bridge running" }),
    ).toBeInTheDocument();
    expect(resumeSpy).toHaveBeenCalledWith("server-henosis");
  });

  it("ignores a bridge mutation result after leaving and returning to its room", async () => {
    const client = new FixtureHenosisClient();
    const stalePause = deferred<RoomBridgeStatus>();
    vi.spyOn(client, "pauseRoomBridge").mockImplementation(
      () => stalePause.promise,
    );
    const firstRoom = primaryRoom();
    const secondRoom = createFixtureRooms().find(
      (candidate) => candidate.serverId === "server-trust",
    );
    if (!secondRoom) {
      throw new Error("The stale bridge test requires the Trust Lab fixture room.");
    }
    const { rerender } = render(
      <RoomDashboard
        client={client}
        room={firstRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    fireEvent.click(await screen.findByRole("tab", { name: "Room" }));
    fireEvent.click(screen.getByRole("button", { name: "Pause bridge" }));

    rerender(
      <RoomDashboard
        client={client}
        room={secondRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    expect(
      await screen.findByRole("heading", { name: secondRoom.name }),
    ).toBeInTheDocument();
    rerender(
      <RoomDashboard
        client={client}
        room={firstRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    expect(
      await screen.findByRole("heading", { name: firstRoom.name }),
    ).toBeInTheDocument();

    await act(async () => {
      stalePause.resolve({
        paused: true,
        desiredRevision: 7,
        activeRevision: 7,
        lastGoodRevision: 7,
        runtimeActivation: "active",
        runtimeErrorCode: null,
        runtimeErrorMessage: null,
      });
      await stalePause.promise;
    });

    expect(
      screen.getByRole("heading", { name: "Bridge running" }),
    ).toBeInTheDocument();
  });

  it("keeps room bridge mutations unavailable without manager permission", async () => {
    const client = new FixtureHenosisClient();
    const room = createFixtureRooms().find(
      (candidate) => candidate.serverId === "server-trust",
    );
    if (!room) {
      throw new Error("The permission test requires the Trust Lab fixture room.");
    }
    await renderDashboard(client, room);

    fireEvent.click(screen.getByRole("tab", { name: "Room" }));

    expect(
      screen.getByText("Bridge controls require room-manager access."),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /bridge/i }),
    ).not.toBeInTheDocument();
  });

  it("does not query dashboard data when cached room context has no current user", () => {
    const client = new FixtureHenosisClient();
    const ownedSpy = vi.spyOn(client, "getMyAgents");
    const rosterSpy = vi.spyOn(client, "getRoomAgentRoster");
    const onReconnect = vi.fn();

    render(
      <RoomDashboard
        client={client}
        room={primaryRoom()}
        currentUserId={undefined}
        onReconnect={onReconnect}
      />,
    );

    expect(
      screen.getByRole("heading", { name: "Reconnect to manage this room" }),
    ).toBeInTheDocument();
    expect(ownedSpy).not.toHaveBeenCalled();
    expect(rosterSpy).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Reconnect" }));
    expect(onReconnect).toHaveBeenCalledOnce();
  });

  it("shows a bounded unavailable state and retries the complete load", async () => {
    const client = new FixtureHenosisClient();
    const rosterSpy = vi
      .spyOn(client, "getRoomAgentRoster")
      .mockRejectedValueOnce(
        new HenosisClientError(
          "unavailable",
          "Room agent controls are temporarily unavailable.",
        ),
      );

    render(
      <RoomDashboard
        client={client}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );

    expect(
      await screen.findByRole("heading", { name: "Room controls unavailable" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent(
      "Room agent controls are temporarily unavailable.",
    );
    fireEvent.click(screen.getByRole("button", { name: "Retry room controls" }));

    expect(await screen.findByRole("tab", { name: "Agents" })).toBeInTheDocument();
    expect(rosterSpy).toHaveBeenCalledTimes(2);
  });

  it("ignores a stale load after the selected room context changes", async () => {
    const client = new FixtureHenosisClient();
    const owned = await client.getMyAgents();
    const staleOwned = deferred<OwnedAgentIdentity[]>();
    const getMyAgents = client.getMyAgents.bind(client);
    vi.spyOn(client, "getMyAgents")
      .mockImplementationOnce(() => staleOwned.promise)
      .mockImplementation(() => getMyAgents());
    const trustRoom = createFixtureRooms().find(
      (candidate) => candidate.serverId === "server-trust",
    );
    if (!trustRoom) {
      throw new Error("The stale-load test requires the Trust Lab fixture room.");
    }
    const { rerender } = render(
      <RoomDashboard
        client={client}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    await waitFor(() => expect(client.getMyAgents).toHaveBeenCalledOnce());

    rerender(
      <RoomDashboard
        client={client}
        room={trustRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    expect(
      await screen.findByText("No agents in this room yet."),
    ).toBeInTheDocument();
    await act(async () => {
      staleOwned.resolve(owned);
      await staleOwned.promise;
    });

    expect(screen.getByText("No agents in this room yet.")).toBeInTheDocument();
    expect(screen.queryByText("3 agents in this room")).not.toBeInTheDocument();
  });

  it("creates an owned identity, refreshes context, and adds one local room draft", async () => {
    const client = new FixtureHenosisClient();
    const createSpy = vi.spyOn(client, "createMyAgent");
    const ownedSpy = vi.spyOn(client, "getMyAgents");
    const rosterSpy = vi.spyOn(client, "getRoomAgentRoster");
    await renderDashboard(client);

    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
    fireEvent.change(screen.getByRole("textbox", { name: "Handle" }), {
      target: { value: "builder" },
    });
    fireEvent.change(screen.getByRole("textbox", { name: "Display name (optional)" }), {
      target: { value: "Builder" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create and add" }));

    expect(await screen.findByRole("heading", { name: "Builder" })).toBeInTheDocument();
    expect(screen.getByText("4 agents in this room")).toBeInTheDocument();
    expect(screen.getByText("Not yet applied")).toBeInTheDocument();
    expect(createSpy).toHaveBeenCalledWith("builder", "Builder");
    expect(ownedSpy).toHaveBeenCalledTimes(2);
    expect(rosterSpy).toHaveBeenCalledTimes(2);
  });

  it("claims a roster-visible identity only after confirmation and refreshes its owner", async () => {
    const client = new FixtureHenosisClient();
    const claimSpy = vi.spyOn(client, "claimAgent");
    const ownedSpy = vi.spyOn(client, "getMyAgents");
    const rosterSpy = vi.spyOn(client, "getRoomAgentRoster");
    await renderDashboard(client);

    expect(
      screen.getByRole("combobox", { name: "Execution harness for Imported scout" }),
    ).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
    fireEvent.click(screen.getByRole("button", { name: "Claim Imported scout" }));
    expect(claimSpy).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Confirm claim" }));

    await waitFor(() => {
      expect(
        screen.getByRole("combobox", { name: "Execution harness for Imported scout" }),
      ).toBeEnabled();
    });
    expect(screen.getByText("3 agents in this room")).toBeInTheDocument();
    expect(claimSpy).toHaveBeenCalledWith("agent-imported");
    expect(ownedSpy).toHaveBeenCalledTimes(2);
    expect(rosterSpy).toHaveBeenCalledTimes(2);
  });

  it("applies one complete normalized roster and polls the accepted revision active", async () => {
    const client = new FixtureHenosisClient();
    const applySpy = vi.spyOn(client, "applyRoomAgentRoster");
    const statusSpy = vi.spyOn(client, "getRoomBridgeStatus");
    await renderDashboard(client);
    vi.useFakeTimers();

    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(applySpy).toHaveBeenCalledWith(
      "server-henosis",
      expect.objectContaining({
        expectedRevision: 2,
        seats: [
          expect.objectContaining({
            seatId: "seat-mira",
            settings: expect.objectContaining({ effort: "high" }),
            position: 0,
          }),
          expect.objectContaining({ seatId: "seat-cinder", position: 1 }),
          expect.objectContaining({ seatId: "seat-imported", position: 2 }),
        ],
      }),
    );
    expect(screen.getByText("Activating revision 3")).toBeInTheDocument();

    await act(async () => vi.advanceTimersByTimeAsync(1_000));
    expect(statusSpy).toHaveBeenCalledTimes(2);
    expect(screen.queryByLabelText("Agent roster changes")).not.toBeInTheDocument();
    expect(screen.getByText("Bridge active")).toBeInTheDocument();
  });

  it("blocks invalid combinations locally, attaches issues to the seat, and discards", async () => {
    const client = new FixtureHenosisClient();
    const applySpy = vi.spyOn(client, "applyRoomAgentRoster");
    await renderDashboard(client);

    fireEvent.change(
      screen.getByRole("combobox", { name: "Execution harness for Mira" }),
      { target: { value: "claude-code" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
    const card = screen.getByRole("heading", { name: "Mira" }).closest("article");
    if (!card) {
      throw new Error("Mira must remain inside an agent seat card.");
    }

    expect(within(card).getByRole("alert")).toHaveTextContent(
      "Choose a model.",
    );
    expect(applySpy).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Discard changes" }));
    expect(
      screen.getByRole("combobox", { name: "Execution harness for Mira" }),
    ).toHaveValue("codex-cli");
    expect(screen.queryByLabelText("Agent roster changes")).not.toBeInTheDocument();
  });

  it("preserves a stale draft for conflict review and restores fetched server truth", async () => {
    const client = new FixtureHenosisClient();
    const originalGetRoster = client.getRoomAgentRoster.bind(client);
    const initial = await originalGetRoster("server-henosis");
    const remote: AgentRosterSnapshot = {
      ...initial,
      desiredRevision: 3,
      seats: initial.seats.map((seat) =>
        seat.seatId === "seat-mira"
          ? { ...seat, enabled: false, configurationRevision: 3 }
          : { ...seat, configurationRevision: 3 },
      ),
    };
    let conflictRaised = false;
    vi.spyOn(client, "applyRoomAgentRoster").mockImplementation(async () => {
      conflictRaised = true;
      throw new HenosisClientError(
        "conflict",
        "The room roster changed.",
        "revision_conflict",
      );
    });
    vi.spyOn(client, "getRoomAgentRoster").mockImplementation(async (serverId) =>
      conflictRaised ? remote : originalGetRoster(serverId),
    );
    await renderDashboard(client);

    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
    expect(
      await screen.findByText("Room changes arrived before your draft was applied."),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
    ).toHaveValue(
      "high",
    );
    expect(screen.queryByRole("button", { name: "Apply roster" })).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Review changes" }));
    expect(screen.getByText("Mine: Setting: effort")).toBeInTheDocument();
    expect(screen.getByText("Server: Participation")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Discard mine" }));
    expect(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
    ).toHaveValue(
      "medium",
    );
    expect(screen.getByRole("switch", { name: "Enable Mira in this room" })).not.toBeChecked();
  });

  it("keeps desired configuration after activation failure and retries without apply", async () => {
    const client = new FixtureHenosisClient();
    client.injectNextAgentActivationFailure(
      "server-henosis",
      "bridge_start_failed",
    );
    const applySpy = vi.spyOn(client, "applyRoomAgentRoster");
    const reconcileSpy = vi.spyOn(client, "reconcileRoomBridge");
    await renderDashboard(client);
    vi.useFakeTimers();

    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
      await Promise.resolve();
      await Promise.resolve();
    });
    await act(async () => vi.advanceTimersByTimeAsync(1_000));

    expect(screen.getByText("Activation failed")).toBeInTheDocument();
    expect(screen.getAllByText("Last good revision 2")).not.toHaveLength(0);
    expect(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
    ).toHaveValue(
      "high",
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Retry activation" }));
      await Promise.resolve();
      await Promise.resolve();
    });

    expect(reconcileSpy).toHaveBeenCalledWith("server-henosis");
    expect(applySpy).toHaveBeenCalledOnce();
    expect(screen.getByText("Activating revision 3")).toBeInTheDocument();
  });

  it("stops bounded pending polling at sixty seconds and offers a manual refresh", async () => {
    const client = new FixtureHenosisClient();
    const originalApply = client.applyRoomAgentRoster.bind(client);
    const originalStatus = client.getRoomBridgeStatus.bind(client);
    let holdPending = false;
    vi.spyOn(client, "applyRoomAgentRoster").mockImplementation(
      async (serverId, request) => {
        const snapshot = await originalApply(serverId, request);
        holdPending = true;
        return snapshot;
      },
    );
    const statusSpy = vi
      .spyOn(client, "getRoomBridgeStatus")
      .mockImplementation(async (serverId) => {
        if (!holdPending) {
          return originalStatus(serverId);
        }
        const roster = await client.getRoomAgentRoster(serverId);
        return {
          paused: false,
          desiredRevision: roster.desiredRevision,
          activeRevision: roster.activeRevision,
          lastGoodRevision: roster.lastGoodRevision,
          runtimeActivation: "pending",
          runtimeErrorCode: null,
          runtimeErrorMessage: null,
        };
      });
    await renderDashboard(client);
    vi.useFakeTimers();

    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
    await act(async () => Promise.resolve());
    await act(async () => vi.advanceTimersByTimeAsync(60_000));

    expect(
      screen.getByText(
        "Activation is taking longer than expected. The desired roster remains pending.",
      ),
    ).toBeInTheDocument();
    const callsAtTimeout = statusSpy.mock.calls.length;
    await act(async () => vi.advanceTimersByTimeAsync(60_000));
    expect(statusSpy).toHaveBeenCalledTimes(callsAtTimeout);
    fireEvent.click(screen.getByRole("button", { name: "Refresh status" }));
    await act(async () => Promise.resolve());
    expect(statusSpy).toHaveBeenCalledTimes(callsAtTimeout + 1);
  });

  it("keeps Discard available after a failed save without losing the draft", async () => {
    const client = new FixtureHenosisClient();
    vi.spyOn(client, "applyRoomAgentRoster").mockRejectedValue(
      new HenosisClientError(
        "unavailable",
        "The room agent controls are temporarily unavailable.",
        "room_agent_unavailable",
      ),
    );
    await renderDashboard(client);

    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));

    expect(
      await screen.findByText("Roster changes were not saved."),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
    ).toHaveValue("high");
    expect(screen.getByRole("button", { name: "Apply roster" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Discard changes" })).toBeEnabled();
  });

  it("ignores a stale apply result after leaving and returning to its room", async () => {
    const client = new FixtureHenosisClient();
    const initial = await client.getRoomAgentRoster("server-henosis");
    const staleApply = deferred<AgentRosterSnapshot>();
    vi.spyOn(client, "applyRoomAgentRoster").mockImplementation(
      () => staleApply.promise,
    );
    const firstRoom = primaryRoom();
    const secondRoom = createFixtureRooms().find(
      (candidate) => candidate.serverId === "server-trust",
    );
    if (!secondRoom) {
      throw new Error("The stale Apply test requires the Trust Lab fixture room.");
    }
    const { rerender } = render(
      <RoomDashboard
        client={client}
        room={firstRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    await screen.findByRole("combobox", { name: "Reasoning effort for Mira" });
    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));

    rerender(
      <RoomDashboard
        client={client}
        room={secondRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    await screen.findByText("No agents in this room yet.");
    rerender(
      <RoomDashboard
        client={client}
        room={firstRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    await screen.findByRole("combobox", { name: "Reasoning effort for Mira" });
    await act(async () => {
      staleApply.resolve({
        ...initial,
        desiredRevision: 3,
        runtimeActivation: "pending",
        seats: initial.seats.map((seat) => ({
          ...seat,
          settings:
            seat.seatId === "seat-mira"
              ? { ...seat.settings, effort: "high" }
              : seat.settings,
          configurationRevision: 3,
          runtimeActivation: "pending",
        })),
      });
      await staleApply.promise;
    });

    expect(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
    ).toHaveValue("medium");
    expect(screen.queryByText("Activating revision 3")).not.toBeInTheDocument();
    expect(screen.getByText("Bridge active")).toBeInTheDocument();
  });

  it("cancels pending activation polling when the selected room changes", async () => {
    const client = new FixtureHenosisClient();
    const originalApply = client.applyRoomAgentRoster.bind(client);
    const originalStatus = client.getRoomBridgeStatus.bind(client);
    let holdPrimaryPending = false;
    vi.spyOn(client, "applyRoomAgentRoster").mockImplementation(
      async (serverId, request) => {
        const snapshot = await originalApply(serverId, request);
        holdPrimaryPending = true;
        return snapshot;
      },
    );
    const statusSpy = vi
      .spyOn(client, "getRoomBridgeStatus")
      .mockImplementation(async (serverId) => {
        if (serverId !== "server-henosis" || !holdPrimaryPending) {
          return originalStatus(serverId);
        }
        const roster = await client.getRoomAgentRoster(serverId);
        return {
          paused: false,
          desiredRevision: roster.desiredRevision,
          activeRevision: roster.activeRevision,
          lastGoodRevision: roster.lastGoodRevision,
          runtimeActivation: "pending",
          runtimeErrorCode: null,
          runtimeErrorMessage: null,
        };
      });
    const secondRoom = createFixtureRooms().find(
      (candidate) => candidate.serverId === "server-trust",
    );
    if (!secondRoom) {
      throw new Error("The polling cancellation test requires the Trust Lab room.");
    }
    const { rerender } = render(
      <RoomDashboard
        client={client}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    await screen.findByRole("combobox", { name: "Reasoning effort for Mira" });
    vi.useFakeTimers();
    fireEvent.change(
      screen.getByRole("combobox", { name: "Reasoning effort for Mira" }),
      { target: { value: "high" } },
    );
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));
      await Promise.resolve();
      await Promise.resolve();
    });
    const primaryCallsBeforeSwitch = statusSpy.mock.calls.filter(
      ([serverId]) => serverId === "server-henosis",
    ).length;

    rerender(
      <RoomDashboard
        client={client}
        room={secondRoom}
        currentUserId="fixture-user"
        onReconnect={vi.fn()}
      />,
    );
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(
      statusSpy.mock.calls.filter(([serverId]) => serverId === "server-henosis"),
    ).toHaveLength(primaryCallsBeforeSwitch);
  });
});

describe("DashboardTabs", () => {
  it("lets a dirty-navigation guard retain the active tab or discard and continue", () => {
    const onSelect = vi.fn();
    const guard = vi.fn(() => false);
    const { rerender } = render(
      <DashboardTabs activeTab="agents" onSelect={onSelect} canSelect={guard} />,
    );

    fireEvent.click(screen.getByRole("tab", { name: "People" }));
    expect(guard).toHaveBeenCalledWith("people");
    expect(onSelect).not.toHaveBeenCalled();

    guard.mockReturnValue(true);
    rerender(
      <DashboardTabs activeTab="agents" onSelect={onSelect} canSelect={guard} />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "People" }));
    expect(onSelect).toHaveBeenCalledWith("people");
  });
});

describe("RoomDetail dashboard presentation", () => {
  it("keeps the conversation mounted while dashboard tabs change", async () => {
    installViewport(1280);
    const client = new FixtureHenosisClient();
    render(
      <RoomDetail
        client={client}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onBack={vi.fn()}
        onReconnect={vi.fn()}
        onUnavailableAction={vi.fn()}
      />,
    );

    const conversation = await screen.findByRole("region", {
      name: "Room conversation",
    });
    expect(
      await screen.findByRole("complementary", { name: "Room dashboard" }),
    ).toBeInTheDocument();
    fireEvent.click(await screen.findByRole("tab", { name: "People" }));

    expect(screen.getByRole("region", { name: "Room conversation" })).toBe(
      conversation,
    );
  });

  it("traps narrow-sheet focus, closes on Escape, and restores its trigger", async () => {
    installViewport(900);
    const client = new FixtureHenosisClient();
    render(
      <RoomDetail
        client={client}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onBack={vi.fn()}
        onReconnect={vi.fn()}
        onUnavailableAction={vi.fn()}
      />,
    );
    await screen.findByRole("region", { name: "Room conversation" });
    const trigger = screen.getByRole("button", { name: "Open room dashboard" });

    fireEvent.click(trigger);
    const dialog = await screen.findByRole("dialog", { name: "Room dashboard" });
    expect(dialog).toHaveAttribute("aria-modal", "true");
    const close = within(dialog).getByRole("button", { name: "Close room controls" });
    expect(close).toHaveFocus();

    const last = within(dialog).getByRole("button", { name: "Add agent identity" });
    last.focus();
    fireEvent.keyDown(last, { key: "Tab" });
    expect(close).toHaveFocus();
    fireEvent.keyDown(close, { key: "Tab", shiftKey: true });
    expect(last).toHaveFocus();

    fireEvent.click(close);
    await waitFor(() => {
      expect(
        screen.queryByRole("dialog", { name: "Room dashboard" }),
      ).not.toBeInTheDocument();
      expect(trigger).toHaveFocus();
    });

    fireEvent.click(trigger);
    const reopened = await screen.findByRole("dialog", { name: "Room dashboard" });
    fireEvent.keyDown(reopened, { key: "Escape" });
    await waitFor(() => expect(trigger).toHaveFocus());
  });

  it("waits for the backdrop click before closing and moves focus into the desktop aside", async () => {
    const viewport = installViewport(900);
    render(
      <RoomDetail
        client={new FixtureHenosisClient()}
        room={primaryRoom()}
        currentUserId="fixture-user"
        onBack={vi.fn()}
        onReconnect={vi.fn()}
        onUnavailableAction={vi.fn()}
      />,
    );
    const trigger = await screen.findByRole("button", {
      name: "Open room dashboard",
    });
    fireEvent.click(trigger);
    const dialog = await screen.findByRole("dialog", { name: "Room dashboard" });
    const backdrop = document.querySelector<HTMLElement>(
      ".room-dashboard-backdrop",
    );
    if (!backdrop) {
      throw new Error("The open narrow dashboard requires a backdrop.");
    }

    fireEvent.mouseDown(backdrop);
    expect(dialog).toBeInTheDocument();
    fireEvent.click(backdrop);
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "Room dashboard" }),
      ).not.toBeInTheDocument(),
    );
    fireEvent.click(trigger);
    await screen.findByRole("dialog", { name: "Room dashboard" });
    await act(async () => viewport.setWidth(1280));

    expect(
      await screen.findByRole("complementary", { name: "Room dashboard" }),
    ).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "Agents" })).toHaveFocus(),
    );
  });

  it("requires explicit confirmation before leaving a dirty dashboard", () => {
    const remain = vi.fn(() => false);
    const discard = vi.fn(() => true);

    expect(confirmDashboardNavigation(true, remain)).toBe(false);
    expect(remain).toHaveBeenCalledOnce();
    expect(confirmDashboardNavigation(true, discard)).toBe(true);
    expect(discard).toHaveBeenCalledOnce();
    expect(confirmDashboardNavigation(false, remain)).toBe(true);
    expect(remain).toHaveBeenCalledOnce();
  });
});
