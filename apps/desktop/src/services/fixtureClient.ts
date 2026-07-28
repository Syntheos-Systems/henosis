/** Explicit fixture adapter used by browser development and component tests. */
import { createFixtureRooms } from "../data/fixtureRooms";
import type {
  BootstrapResult,
  HenosisClient,
  RiftConnectionInput,
  RoomDirectorySnapshot,
  SanitizedConnection,
} from "./henosisClient";

/** Browser-only client that never claims fixture rooms came from a live Rift. */
export class FixtureHenosisClient implements HenosisClient {
  /** Return a fixture-backed connected directory for visual development. */
  async bootstrap(): Promise<BootstrapResult> {
    return {
      directory: this.snapshot(),
      requiresAuthentication: false,
    };
  }

  /** Accept GUI credentials without retaining them and return fixture data. */
  async connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot> {
    return this.snapshot({
      endpoint: input.endpoint,
      username: input.username,
      userId: "fixture-user",
      displayName: input.username || "Preview operator",
    });
  }

  /** Refresh fixture timestamps and room data. */
  async refresh(): Promise<RoomDirectorySnapshot> {
    return this.snapshot();
  }

  /** End the fixture session without side effects. */
  async disconnect(): Promise<void> {
    return Promise.resolve();
  }

  /** Assemble a visibly fixture-backed snapshot. */
  private snapshot(
    connection: SanitizedConnection = {
      endpoint: "http://127.0.0.1:4010",
      username: "zan",
      userId: "fixture-user",
      displayName: "Zan",
    },
  ): RoomDirectorySnapshot {
    return {
      connection,
      rooms: createFixtureRooms(),
      source: "fixture",
      fetchedAt: new Date().toISOString(),
      connected: true,
    };
  }
}
