/** End-to-end component tests for room selection and conversation integration. */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { createFixtureRooms } from "./data/fixtureRooms";
import { FixtureHenosisClient } from "./services/fixtureClient";
import type {
  BootstrapResult,
  HenosisClient,
  RiftConnectionInput,
  RoomEventListener,
  RoomDirectorySnapshot,
} from "./services/henosisClient";

/** Create one deterministic fixture directory for App tests. */
function fixtureDirectory(): RoomDirectorySnapshot {
  return {
    connection: {
      endpoint: "http://127.0.0.1:4010",
      username: "operator",
      userId: "user-operator",
      displayName: "Operator",
    },
    rooms: createFixtureRooms(new Date("2026-07-26T18:00:00.000Z")),
    source: "fixture",
    fetchedAt: "2026-07-26T18:00:00.000Z",
    connected: true,
  };
}

/** Minimal injected client that records App requests without native IPC. */
class TestClient extends FixtureHenosisClient implements HenosisClient {
  /** Bootstrap response supplied by each test. */
  readonly bootstrapResult: BootstrapResult;
  /** Connect spy shared with assertions. */
  readonly connectSpy = vi.fn();
  /** Refresh spy shared with assertions. */
  readonly refreshSpy = vi.fn();
  /** Room-open spy retaining each room and one-use generation identifier. */
  readonly openRoomSpy = vi.fn();
  /** Room-close spy proving exact generation release during navigation. */
  readonly closeRoomSpy = vi.fn();
  /** Subscription spy proving the workspace registers one native listener. */
  readonly subscribeRoomEventsSpy = vi.fn();
  /** Listener cleanup spy proving old room events cannot survive navigation. */
  readonly unlistenRoomEventsSpy = vi.fn();

  /** Create a client with a selected bootstrap state. */
  constructor(bootstrapResult: BootstrapResult) {
    super();
    this.bootstrapResult = bootstrapResult;
  }

  /** Return the selected initial state. */
  async bootstrap(): Promise<BootstrapResult> {
    return this.bootstrapResult;
  }

  /** Record credentials and return a fixture directory. */
  async connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot> {
    this.connectSpy(input);
    return fixtureDirectory();
  }

  /** Record a refresh and return the current fixture directory. */
  async refresh(): Promise<RoomDirectorySnapshot> {
    this.refreshSpy();
    return fixtureDirectory();
  }

  /** Record and delegate one sanitized fixture room generation open. */
  async openRoom(roomId: string, streamId: string) {
    this.openRoomSpy(roomId, streamId);
    return super.openRoom(roomId, streamId);
  }

  /** Record and delegate exact fixture generation cleanup. */
  async closeRoom(roomId: string, streamId: string): Promise<void> {
    this.closeRoomSpy(roomId, streamId);
    return super.closeRoom(roomId, streamId);
  }

  /** Wrap the fixture event listener with observable idempotent cleanup. */
  async subscribeRoomEvents(listener: RoomEventListener) {
    this.subscribeRoomEventsSpy(listener);
    const unlisten = await super.subscribeRoomEvents(listener);
    let listening = true;
    return () => {
      if (listening) {
        listening = false;
        this.unlistenRoomEventsSpy();
        unlisten();
      }
    };
  }

  /** Satisfy the client contract without remote state. */
  async disconnect(): Promise<void> {
    return Promise.resolve();
  }
}

describe("App", () => {
  it("opens to Rooms, pins the newest room, and identifies fixture data", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
    });

    render(<App client={client} />);

    expect(
      await screen.findByRole("heading", { name: "Return to the current." }),
    ).toBeInTheDocument();
    expect(screen.getByText("#orchard")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Browser preview");
    expect(screen.queryByRole("heading", { name: "Athena" })).not.toBeInTheDocument();
  });

  it("opens the primary room conversation beside its dashboard and closes it on return", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
    });
    render(<App client={client} />);

    fireEvent.click(
      await screen.findByRole("button", { name: "Continue room" }),
    );

    expect(
      screen.getByRole("heading", { name: "orchard" }),
    ).toBeInTheDocument();
    await waitFor(() => expect(client.openRoomSpy).toHaveBeenCalledOnce());
    expect(client.subscribeRoomEventsSpy).toHaveBeenCalledOnce();

    const [roomId, streamId] = client.openRoomSpy.mock.calls[0];
    expect(roomId).toBe("room-orchard");
    expect(streamId).toMatch(/^[a-f0-9]{32,}$/);

    const conversation = await screen.findByRole("region", {
      name: "Room conversation",
    });
    const timeline = screen.getByRole("log", { name: "Room message timeline" });
    const dashboard = screen.getByRole("complementary", {
      name: "Room dashboard",
    });
    expect(conversation).toContainElement(timeline);
    expect(
      conversation.compareDocumentPosition(dashboard) &
        Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "Open room dashboard" }),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "All rooms" }));
    expect(
      screen.getByRole("heading", { name: "Return to the current." }),
    ).toBeInTheDocument();
    await waitFor(() => {
      expect(client.unlistenRoomEventsSpy).toHaveBeenCalledOnce();
      expect(client.closeRoomSpy).toHaveBeenCalledWith(roomId, streamId);
    });
  });

  it("releases the old room generation before opening a different room", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
    });
    render(<App client={client} />);

    fireEvent.click(
      await screen.findByRole("button", { name: "Continue room" }),
    );
    await waitFor(() => expect(client.openRoomSpy).toHaveBeenCalledOnce());
    const firstStreamId = client.openRoomSpy.mock.calls[0][1];

    fireEvent.click(screen.getByRole("button", { name: "All rooms" }));
    await waitFor(() => {
      expect(client.unlistenRoomEventsSpy).toHaveBeenCalledOnce();
      expect(client.closeRoomSpy).toHaveBeenCalledWith(
        "room-orchard",
        firstStreamId,
      );
    });

    const search = screen.getByLabelText(
      "Search rooms, servers, messages, and participants",
    );
    fireEvent.change(search, { target: { value: "Trust Lab" } });
    fireEvent.click(screen.getByRole("button", { name: "Continue room" }));

    await waitFor(() => expect(client.openRoomSpy).toHaveBeenCalledTimes(2));
    const [secondRoomId, secondStreamId] = client.openRoomSpy.mock.calls[1];
    expect(secondRoomId).toBe("room-governance");
    expect(secondStreamId).not.toBe(firstStreamId);
    expect(client.subscribeRoomEventsSpy).toHaveBeenCalledTimes(2);
    expect(client.closeRoomSpy.mock.invocationCallOrder[0]).toBeLessThan(
      client.openRoomSpy.mock.invocationCallOrder[1],
    );
    expect(
      screen.getByRole("heading", { name: "governance" }),
    ).toBeInTheDocument();
  });

  it("uses the first-run form when no directory exists", async () => {
    const client = new TestClient({ requiresAuthentication: true });
    render(<App client={client} />);

    expect(await screen.findByLabelText("Rift endpoint")).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText("Username"), {
      target: { value: "operator" },
    });
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: "secret-value" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect and view rooms" }));

    await waitFor(() => expect(client.connectSpy).toHaveBeenCalledOnce());
    expect(
      await screen.findByRole("heading", { name: "Return to the current." }),
    ).toBeInTheDocument();
  });
});
