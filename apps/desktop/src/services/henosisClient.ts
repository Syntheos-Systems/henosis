/**
 * Typed native boundary for the Henosis webview.
 *
 * These response types intentionally contain no access-token or refresh-token
 * fields. Rift credentials and tokens remain in the Tauri Rust process.
 */
import type { DirectorySource, RoomSummary } from "../domain/rooms";

/** Credentials collected by the first-run connection form. */
export interface RiftConnectionInput {
  /** Base HTTP or HTTPS URL for Rift. */
  endpoint: string;
  /** Rift login handle. */
  username: string;
  /** Rift password passed directly to the native process. */
  password: string;
}

/** Non-secret profile information safe to render and persist. */
export interface ConnectionProfile {
  /** Normalized Rift base URL. */
  endpoint: string;
  /** Rift login handle. */
  username: string;
}

/** Authenticated Rift identity safe to expose to the webview. */
export interface SanitizedConnection extends ConnectionProfile {
  /** Stable Rift user identifier. */
  userId: string;
  /** Human-facing user name. */
  displayName: string;
}

/** Room-directory payload returned by live, cached, or fixture adapters. */
export interface RoomDirectorySnapshot {
  /** Sanitized authenticated identity when a live session exists. */
  connection?: SanitizedConnection;
  /** Sorted room summaries. */
  rooms: RoomSummary[];
  /** Whether room data is live, cached, or fixture-backed. */
  source: DirectorySource;
  /** ISO timestamp for the snapshot. */
  fetchedAt: string;
  /** True only while the native process has an authenticated Rift session. */
  connected: boolean;
}

/** Initial state returned before Henosis chooses setup or room selection. */
export interface BootstrapResult {
  /** A saved non-secret profile that can prefill first-run controls. */
  savedProfile?: ConnectionProfile;
  /** Cached or live room data when native state can provide it. */
  directory?: RoomDirectorySnapshot;
  /** True when a person must authenticate before live refreshes. */
  requiresAuthentication: boolean;
}

/** Stable error categories used for actionable GUI recovery states. */
export type ClientErrorKind =
  | "authentication"
  | "connection-required"
  | "network"
  | "protocol"
  | "storage"
  | "validation"
  | "unknown";

/** Structured transport error rendered by Henosis recovery states. */
export class HenosisClientError extends Error {
  /** Machine-readable category for choosing a recovery action. */
  readonly kind: ClientErrorKind;

  /** Create a structured client error without retaining sensitive request data. */
  constructor(kind: ClientErrorKind, message: string) {
    super(message);
    this.name = "HenosisClientError";
    this.kind = kind;
  }
}

/** Operations the room selector is allowed to request from the native process. */
export interface HenosisClient {
  /** Inspect saved profile and cached/native session state. */
  bootstrap(): Promise<BootstrapResult>;
  /** Authenticate and return the first live room snapshot. */
  connect(input: RiftConnectionInput): Promise<RoomDirectorySnapshot>;
  /** Refresh rooms through the already authenticated native session. */
  refresh(): Promise<RoomDirectorySnapshot>;
  /** End the native Rift session and clear secret process state. */
  disconnect(): Promise<void>;
}

/** Serialized Tauri command error shape returned by the Rust boundary. */
interface NativeCommandError {
  /** Machine-readable error category. */
  kind?: ClientErrorKind;
  /** Human-readable safe error text. */
  message?: string;
}

/** Convert any adapter rejection into a stable, user-safe error. */
export function normalizeClientError(error: unknown): HenosisClientError {
  if (error instanceof HenosisClientError) {
    return error;
  }

  if (typeof error === "object" && error !== null) {
    const candidate = error as NativeCommandError;
    if (candidate.kind && candidate.message) {
      return new HenosisClientError(candidate.kind, candidate.message);
    }
  }

  if (typeof error === "string" && error.trim().length > 0) {
    try {
      const parsed = JSON.parse(error) as NativeCommandError;
      if (parsed.kind && parsed.message) {
        return new HenosisClientError(parsed.kind, parsed.message);
      }
    } catch {}
  }

  return new HenosisClientError(
    "unknown",
    "Henosis could not complete that operation. Try again or reconnect to Rift.",
  );
}
