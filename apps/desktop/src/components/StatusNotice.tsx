/** Compact data-provenance and recovery notices for the room selector. */
import { CloudOff, FlaskConical, Info, Wifi } from "lucide-react";
import type { DirectorySource } from "../domain/rooms";

/** Inputs controlling selector provenance messaging and recovery action. */
export interface StatusNoticeProps {
  /** Current directory provenance. */
  source: DirectorySource;
  /** Whether native Rift session state is authenticated. */
  connected: boolean;
  /** Open first-run setup when reconnection is required. */
  onReconnect(): void;
}

/** Explain whether the visible rooms are live, cached, or fixture-backed. */
export function StatusNotice({
  source,
  connected,
  onReconnect,
}: StatusNoticeProps) {
  if (source === "fixture") {
    return (
      <div className="status-notice status-notice-fixture" role="status">
        <FlaskConical aria-hidden="true" />
        <p>
          <strong>Browser preview.</strong> These rooms are fixtures, not live
          Rift data.
        </p>
      </div>
    );
  }

  if (source === "cached" || !connected) {
    return (
      <div className="status-notice status-notice-offline" role="status">
        <CloudOff aria-hidden="true" />
        <p>
          <strong>Showing cached rooms.</strong> Rift is not connected, so
          activity may be out of date.
        </p>
        <button className="text-button" type="button" onClick={onReconnect}>
          Reconnect
        </button>
      </div>
    );
  }

  return (
    <div className="status-notice status-notice-live" role="status">
      <Wifi aria-hidden="true" />
      <p>
        <strong>Live from Rift.</strong> Room activity is current.
      </p>
      <Info className="notice-trailer" aria-hidden="true" />
    </div>
  );
}
