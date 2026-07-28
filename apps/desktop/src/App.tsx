/** Henosis application state machine for setup, room selection, and room entry. */
import { useEffect, useState } from "react";
import type { RoomSummary } from "./domain/rooms";
import { AppShell } from "./components/AppShell";
import { ConnectionSetup } from "./components/ConnectionSetup";
import { RoomDetail } from "./components/RoomDetail";
import { RoomDirectory } from "./components/RoomDirectory";
import { createHenosisClient } from "./services/client";
import type {
  ConnectionProfile,
  HenosisClient,
  RiftConnectionInput,
  RoomDirectorySnapshot,
} from "./services/henosisClient";
import { normalizeClientError } from "./services/henosisClient";

/** Default runtime client chosen once for the lifetime of the webview. */
const DEFAULT_CLIENT = createHenosisClient();

/** Optional dependency injection used by component tests. */
export interface AppProps {
  /** Native or deterministic Henosis adapter. */
  client?: HenosisClient;
}

/** Render the complete slice 1 Henosis desktop experience. */
export function App({ client = DEFAULT_CLIENT }: AppProps) {
  const [loading, setLoading] = useState(true);
  const [connecting, setConnecting] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [showSetup, setShowSetup] = useState(false);
  const [profile, setProfile] = useState<ConnectionProfile>();
  const [directory, setDirectory] = useState<RoomDirectorySnapshot>();
  const [selectedRoom, setSelectedRoom] = useState<RoomSummary>();
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();

  useEffect(() => {
    let active = true;

    /** Load saved native state without applying results after unmount. */
    async function loadBootstrap() {
      try {
        const result = await client.bootstrap();
        if (!active) {
          return;
        }
        setProfile(result.savedProfile);
        setDirectory(result.directory);
        setShowSetup(!result.directory);
      } catch (bootstrapError) {
        if (active) {
          setError(normalizeClientError(bootstrapError).message);
          setShowSetup(true);
        }
      } finally {
        if (active) {
          setLoading(false);
        }
      }
    }

    void loadBootstrap();
    return () => {
      active = false;
    };
  }, [client]);

  /** Authenticate through the native boundary and reveal the live room directory. */
  async function handleConnect(input: RiftConnectionInput) {
    setConnecting(true);
    setError(undefined);
    try {
      const connectedDirectory = await client.connect(input);
      setDirectory(connectedDirectory);
      setProfile({ endpoint: input.endpoint, username: input.username });
      setShowSetup(false);
    } catch (connectionError) {
      setError(normalizeClientError(connectionError).message);
    } finally {
      setConnecting(false);
    }
  }

  /** Refresh activity while preserving the currently visible snapshot on failure. */
  async function handleRefresh() {
    setRefreshing(true);
    setNotice(undefined);
    try {
      setDirectory(await client.refresh());
    } catch (refreshError) {
      const normalized = normalizeClientError(refreshError);
      setNotice(normalized.message);
      if (
        normalized.kind === "authentication" ||
        normalized.kind === "connection-required"
      ) {
        setShowSetup(true);
      }
    } finally {
      setRefreshing(false);
    }
  }

  /** Enter a selected Rift room without leaving the Henosis shell. */
  function handleOpenRoom(room: RoomSummary) {
    setSelectedRoom(room);
    setNotice(undefined);
  }

  /** Return to the room selector and preserve its in-memory directory. */
  function handleRooms() {
    setSelectedRoom(undefined);
    setShowSetup(false);
  }

  /** Display an honest boundary for work scheduled after slice 1. */
  function handleDeferredAction(action: string) {
    setNotice(
      `${action} is not wired in this slice. It will land as a visible Henosis control, not a terminal workaround.`,
    );
  }

  if (loading) {
    return <LoadingScreen />;
  }

  if (showSetup || !directory) {
    return (
      <ConnectionSetup
        profile={profile}
        busy={connecting}
        error={error}
        onConnect={handleConnect}
      />
    );
  }

  return (
    <AppShell
      directory={directory}
      connection={directory.connection}
      onRooms={handleRooms}
      onDeferredWorkspace={handleDeferredAction}
    >
      {notice ? (
        <div className="slice-notice" role="status">
          <p>{notice}</p>
          <button type="button" onClick={() => setNotice(undefined)}>
            Dismiss
          </button>
        </div>
      ) : null}

      {selectedRoom ? (
        <RoomDetail
          room={selectedRoom}
          onBack={handleRooms}
          onDeferredAction={handleDeferredAction}
        />
      ) : (
        <RoomDirectory
          directory={directory}
          refreshing={refreshing}
          onOpenRoom={handleOpenRoom}
          onRefresh={handleRefresh}
          onReconnect={() => setShowSetup(true)}
          onDeferredAction={handleDeferredAction}
        />
      )}
    </AppShell>
  );
}

/** Render a calm skeleton while native profile and cache state load. */
function LoadingScreen() {
  return (
    <main className="loading-screen" aria-label="Loading Henosis">
      <span className="brand-mark loading-mark" aria-hidden="true">
        <i />
        <i />
        <i />
      </span>
      <p>Gathering rooms</p>
      <div className="loading-line" aria-hidden="true">
        <span />
      </div>
    </main>
  );
}
