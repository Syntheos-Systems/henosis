/** End-to-end component tests for room selection and conversation integration. */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { createFixtureRooms } from "./data/fixtureRooms";
import { FixtureHenosisClient } from "./services/fixtureClient";
import { HenosisClientError } from "./services/henosisClient";
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

/** Force RoomDetail into its modal dashboard presentation and return a restorer. */
function installNarrowDashboardViewport(): () => void {
  const originalMatchMedia = window.matchMedia;
  const mediaQueryList: MediaQueryList = {
    matches: false,
    media: "(min-width: 1180px)",
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(() => false),
  };
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    writable: true,
    value: vi.fn(() => mediaQueryList),
  });
  return () => {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      writable: true,
      value: originalMatchMedia,
    });
  };
}

/** Minimal injected client that records App requests without native IPC. */
class TestClient extends FixtureHenosisClient implements HenosisClient {
  /** Bootstrap response supplied by each test. */
  readonly bootstrapResult: BootstrapResult;
  /** Optional connection failure used by recovery-path tests. */
  readonly connectFailure?: unknown;
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
  constructor(bootstrapResult: BootstrapResult, connectFailure?: unknown) {
    super();
    this.bootstrapResult = bootstrapResult;
    this.connectFailure = connectFailure;
  }

  /** Return the selected initial state. */
  async bootstrap(): Promise<BootstrapResult> {
    return this.bootstrapResult;
  }

  /** Record credentials and return a fixture directory. */
  async connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot> {
    this.connectSpy(input);
    if (this.connectFailure) {
      throw this.connectFailure;
    }
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
      experience: "desktop",
    });

    render(<App client={client} />);

    expect(
      await screen.findByRole("heading", { name: "Return to the current." }),
    ).toBeInTheDocument();
    expect(screen.getByText("#orchard")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Browser preview");
    expect(screen.queryByRole("heading", { name: "Athena" })).not.toBeInTheDocument();
  });

  it("boots into the installer-selected spatial profile with direct navigation parity", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
      experience: "spatial",
    });

    render(<App client={client} />);

    expect(
      await screen.findByRole("heading", {
        name: "Rooms are places. Agents have a seat.",
      }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("group", { name: "Interactive spatial room field" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("complementary", { name: "Spatial room index" }),
    ).toBeInTheDocument();
    expect(
      screen.getAllByRole("button", { name: "Enter room orchard" }),
    ).toHaveLength(1);
  });

  it("switches renderers without reconnecting or losing the selected room", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
      experience: "desktop",
    });
    const experienceSpy = vi.spyOn(client, "setExperience");
    render(<App client={client} />);

    fireEvent.click(
      await screen.findByRole("button", {
        name: "Chat: Rooms and conversation stay in focus.",
      }),
    );
    expect(
      await screen.findByRole("heading", {
        name: "Conversation without the control room.",
      }),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /orchard/i }));
    await screen.findByRole("region", { name: "Room conversation" });
    fireEvent.click(
      screen.getByRole("button", {
        name: "Desktop: Complete room, agent, and runtime controls.",
      }),
    );

    expect(
      await screen.findByRole("heading", { name: "orchard" }),
    ).toBeInTheDocument();
    expect(experienceSpy).toHaveBeenNthCalledWith(1, "chat");
    expect(experienceSpy).toHaveBeenNthCalledWith(2, "desktop");
    expect(client.connectSpy).not.toHaveBeenCalled();
  });

  it("opens the primary room conversation beside its dashboard and closes it on return", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
      experience: "desktop",
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
      experience: "desktop",
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

  it("applies one complete owned-agent roster and reopens the sheet with saved state", async () => {
    const restoreViewport = installNarrowDashboardViewport();
    try {
      const client = new FixtureHenosisClient();
      const applySpy = vi.spyOn(client, "applyRoomAgentRoster");
      render(<App client={client} />);

      fireEvent.click(await screen.findByRole("button", { name: "Continue room" }));
      fireEvent.click(screen.getByRole("button", { name: "Open room dashboard" }));
      await screen.findByRole("heading", { name: "Agent topology" });

      fireEvent.click(screen.getByRole("button", { name: "Add agent identity" }));
      fireEvent.click(screen.getByRole("button", { name: "Add Lumen to room" }));
      fireEvent.change(screen.getByRole("combobox", { name: "Execution harness for Lumen" }), {
        target: { value: "codex-cli" },
      });
      fireEvent.change(screen.getByRole("combobox", { name: "Model for Lumen" }), {
        target: { value: "gpt-5.6-sol" },
      });
      fireEvent.change(screen.getByRole("combobox", { name: "Reasoning effort for Lumen" }), {
        target: { value: "high" },
      });
      fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));

      await waitFor(() => expect(applySpy).toHaveBeenCalledOnce());
      expect(applySpy).toHaveBeenCalledWith(
        "server-henosis",
        expect.objectContaining({
          expectedRevision: 2,
          seats: expect.arrayContaining([
            expect.objectContaining({
              agentIdentityId: "agent-lumen",
              harnessKey: "codex-cli",
              modelKey: "gpt-5.6-sol",
              settings: { effort: "high" },
            }),
          ]),
        }),
      );
      expect(applySpy.mock.calls[0][1].seats).toHaveLength(4);
      expect(screen.getByText("Activating revision 3")).toBeInTheDocument();
      expect(
        await screen.findAllByText("Active · revision 3", {}, { timeout: 2_500 }),
      ).toHaveLength(4);

      fireEvent.click(screen.getByRole("button", { name: "Close room controls" }));
      expect(screen.getByRole("region", { name: "Room conversation" })).toBeInTheDocument();
      expect(screen.queryByRole("dialog", { name: "Room dashboard" })).not.toBeInTheDocument();
      fireEvent.click(screen.getByRole("button", { name: "Open room dashboard" }));

      expect(await screen.findByRole("heading", { name: "Lumen" })).toBeInTheDocument();
      expect(screen.getByRole("combobox", { name: "Execution harness for Lumen" })).toHaveValue(
        "codex-cli",
      );
      expect(screen.getByRole("combobox", { name: "Model for Lumen" })).toHaveValue(
        "gpt-5.6-sol",
      );
      expect(screen.getByRole("combobox", { name: "Reasoning effort for Lumen" })).toHaveValue(
        "high",
      );
    } finally {
      restoreViewport();
    }
  });

  it("keeps the desired roster and last-good revision visible after activation fails", async () => {
    const client = new FixtureHenosisClient();
    client.injectNextAgentActivationFailure("server-henosis", "fixture_activation_failed");
    const applySpy = vi.spyOn(client, "applyRoomAgentRoster");
    render(<App client={client} />);

    fireEvent.click(await screen.findByRole("button", { name: "Continue room" }));
    const effort = await screen.findByRole("combobox", { name: "Reasoning effort for Mira" });
    fireEvent.change(effort, { target: { value: "high" } });
    fireEvent.click(screen.getByRole("button", { name: "Apply roster" }));

    await waitFor(() => expect(applySpy).toHaveBeenCalledOnce());
    expect(screen.getByText("Activating revision 3")).toBeInTheDocument();
    expect(await screen.findByText("Activation failed", {}, { timeout: 2_500 })).toBeInTheDocument();
    expect(screen.getAllByText("Failed · fixture_activation_failed")).toHaveLength(3);
    expect(screen.getAllByText("Last good revision 2").length).toBeGreaterThan(0);
    expect(effort).toHaveValue("high");
  });

  it("uses the first-run form when no directory exists", async () => {
    const client = new TestClient({
      requiresAuthentication: true,
      experience: "desktop",
    });
    render(<App client={client} />);

    expect(await screen.findByLabelText("Rift endpoint")).toBeInTheDocument();
    expect(screen.getByLabelText("Rift endpoint")).toHaveValue("");
    fireEvent.change(screen.getByLabelText("Rift endpoint"), {
      target: { value: "https://rift.example.test" },
    });
    fireEvent.change(screen.getByLabelText("Username"), {
      target: { value: "operator" },
    });
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: "secret-value" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect and open rooms" }));

    await waitFor(() => expect(client.connectSpy).toHaveBeenCalledOnce());
    expect(client.connectSpy).toHaveBeenCalledWith({
      endpoint: "https://rift.example.test",
      username: "operator",
      password: "secret-value",
    });
    expect(
      await screen.findByRole("heading", { name: "Return to the current." }),
    ).toBeInTheDocument();
  });

  it("preserves non-secret fields and clears the password after App rejects a connection", async () => {
    const client = new TestClient(
      { requiresAuthentication: true, experience: "desktop" },
      new HenosisClientError("authentication", "Rift rejected that account."),
    );
    render(<App client={client} />);

    const endpoint = await screen.findByLabelText("Rift endpoint");
    fireEvent.change(endpoint, {
      target: { value: "https://rift.example.test" },
    });
    fireEvent.change(screen.getByLabelText("Username"), {
      target: { value: "operator" },
    });
    fireEvent.change(screen.getByLabelText("Password"), {
      target: { value: "rejected-password" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Connect and open rooms" }));

    await waitFor(() => expect(screen.getByLabelText("Password")).toHaveValue(""));
    expect(endpoint).toHaveValue("https://rift.example.test");
    expect(screen.getByLabelText("Username")).toHaveValue("operator");
    expect(screen.getByRole("alert")).toHaveTextContent("Rift rejected that account.");
    expect(screen.getByLabelText("Password")).toHaveFocus();
  });
});
