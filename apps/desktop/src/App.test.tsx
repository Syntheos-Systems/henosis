/** End-to-end component tests for the slice 1 room-first experience. */
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { createFixtureRooms } from "./data/fixtureRooms";
import { FixtureHenosisClient } from "./services/fixtureClient";
import type {
  BootstrapResult,
  HenosisClient,
  RiftConnectionInput,
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

  it("searches room context and enters a selected room through the GUI", async () => {
    const client = new TestClient({
      directory: fixtureDirectory(),
      requiresAuthentication: false,
    });
    render(<App client={client} />);

    const search = await screen.findByLabelText(
      "Search rooms, servers, messages, and participants",
    );
    fireEvent.change(search, { target: { value: "Trust Lab" } });

    expect(screen.getByText("#governance")).toBeInTheDocument();
    expect(screen.queryByText("#orchard")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "Continue room" }));

    expect(
      screen.getByRole("heading", { name: "governance" }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("The room is visible. Full conversation sync is next."),
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "All rooms" }));
    expect(
      screen.getByRole("heading", { name: "Return to the current." }),
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
