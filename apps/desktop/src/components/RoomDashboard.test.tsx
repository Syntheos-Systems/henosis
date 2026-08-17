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

    const last = within(dialog).getByRole("tabpanel", { name: "Agents" });
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
