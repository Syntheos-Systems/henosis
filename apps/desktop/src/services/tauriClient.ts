/** Tauri command adapter for the production Henosis desktop runtime. */
import { invoke } from "@tauri-apps/api/core";
import type {
  BootstrapResult,
  HenosisClient,
  RiftConnectionInput,
  RoomDirectorySnapshot,
} from "./henosisClient";
import { normalizeClientError } from "./henosisClient";

/** Client whose calls terminate at the Rust process instead of Rift directly. */
export class TauriHenosisClient implements HenosisClient {
  /** Load saved profile, cache, and any active native session. */
  async bootstrap(): Promise<BootstrapResult> {
    try {
      return await invoke<BootstrapResult>("bootstrap");
    } catch (error) {
      throw normalizeClientError(error);
    }
  }

  /** Pass credentials once to Rust and receive only sanitized room data. */
  async connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot> {
    try {
      return await invoke<RoomDirectorySnapshot>("connect_rift", { input });
    } catch (error) {
      throw normalizeClientError(error);
    }
  }

  /** Refresh room summaries using the token held in native process state. */
  async refresh(): Promise<RoomDirectorySnapshot> {
    try {
      return await invoke<RoomDirectorySnapshot>("get_room_directory");
    } catch (error) {
      throw normalizeClientError(error);
    }
  }

  /** Clear native token state and ask Rift to end refresh sessions later. */
  async disconnect(): Promise<void> {
    try {
      await invoke<void>("disconnect_rift");
    } catch (error) {
      throw normalizeClientError(error);
    }
  }
}
