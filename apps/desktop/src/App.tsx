/** Henosis application state machine for setup, room selection, and room entry. */
import { useEffect, useState } from "react";
import type { RoomSummary } from "./domain/rooms";
import {
  DEFAULT_DESKTOP_EXPERIENCE,
  normalizeDesktopExperience,
} from "./domain/experience";
import type { DesktopExperienceProfile } from "./domain/experience";
import { AppShell } from "./components/AppShell";
import { ChatWorkspace } from "./components/ChatWorkspace";
import { ConnectionSetup } from "./components/ConnectionSetup";
import { ExperienceSelector } from "./components/ExperienceSelector";
import { RoomDetail } from "./components/RoomDetail";
import { RoomDirectory } from "./components/RoomDirectory";
import { SpatialWorkspace } from "./components/SpatialWorkspace";
import { createHenosisClient } from "./services/client";
import type {
  ConnectionProfile,
  HenosisClientError,
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

/** Render the room-first Henosis shell and integrated conversation workspace. */
export function App({ client = DEFAULT_CLIENT }: AppProps) {
  const [loading, setLoading] = useState(true);
  const [connecting, setConnecting] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [showSetup, setShowSetup] = useState(false);
  const [profile, setProfile] = useState<ConnectionProfile>();
  const [directory, setDirectory] = useState<RoomDirectorySnapshot>();
  const [selectedRoom, setSelectedRoom] = useState<RoomSummary>();
  const [error, setError] = useState<HenosisClientError>();
  const [notice, setNotice] = useState<string>();
  const [experience, setExperience] = useState<DesktopExperienceProfile>(
    DEFAULT_DESKTOP_EXPERIENCE,
  );
  const [savingExperience, setSavingExperience] = useState(false);

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
        setExperience(normalizeDesktopExperience(result.experience));
        setShowSetup(!result.directory);
      } catch (bootstrapError) {
        if (active) {
          setError(normalizeClientError(bootstrapError));
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
      const normalized = normalizeClientError(connectionError);
      setError(normalized);
      throw normalized;
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

  /** Return to setup without carrying an obsolete connection failure. */
  function handleReconnect() {
    setError(undefined);
    setShowSetup(true);
  }

  /** Explain when the current build does not expose a requested control. */
  function handleUnavailableAction(action: string) {
    setNotice(`${action} is not available in this build.`);
  }

  /** Persist and activate one renderer without reconnecting or clearing room state. */
  async function handleExperienceChange(next: DesktopExperienceProfile) {
    if (next === experience || savingExperience) {
      return;
    }
    setSavingExperience(true);
    setNotice(undefined);
    try {
      setExperience(normalizeDesktopExperience(await client.setExperience(next)));
    } catch (preferenceError) {
      setNotice(normalizeClientError(preferenceError).message);
    } finally {
      setSavingExperience(false);
    }
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

  const experienceSelector = (
    <ExperienceSelector
      value={experience}
      busy={savingExperience}
      onChange={handleExperienceChange}
    />
  );
  const featureNotice = notice ? (
    <FeatureNotice message={notice} onDismiss={() => setNotice(undefined)} />
  ) : null;

  if (experience === "chat") {
    return (
      <>
        {featureNotice}
        <ChatWorkspace
          client={client}
          directory={directory}
          selectedRoom={selectedRoom}
          onOpenRoom={handleOpenRoom}
          onOpenDesktop={() => void handleExperienceChange("desktop")}
          experienceSelector={experienceSelector}
        />
      </>
    );
  }

  if (experience === "spatial" && !selectedRoom) {
    return (
      <>
        {featureNotice}
        <SpatialWorkspace
          directory={directory}
          onOpenRoom={handleOpenRoom}
          experienceSelector={experienceSelector}
        />
      </>
    );
  }

  return (
    <AppShell
      directory={directory}
      connection={directory.connection}
      onRooms={handleRooms}
      onUnavailableWorkspace={handleUnavailableAction}
      experienceSelector={experienceSelector}
    >
      {featureNotice}

      {selectedRoom ? (
        <RoomDetail
          client={client}
          room={selectedRoom}
          currentUserId={directory.connection?.userId}
          onBack={handleRooms}
          onReconnect={handleReconnect}
          onUnavailableAction={handleUnavailableAction}
        />
      ) : (
        <RoomDirectory
          directory={directory}
          refreshing={refreshing}
          onOpenRoom={handleOpenRoom}
          onRefresh={handleRefresh}
          onReconnect={handleReconnect}
          onUnavailableAction={handleUnavailableAction}
        />
      )}
    </AppShell>
  );
}

/** Render a dismissible status message above every graphical experience. */
function FeatureNotice({
  message,
  onDismiss,
}: {
  /** Human-readable status or failure detail. */
  message: string;
  /** Clear the current notice. */
  onDismiss: () => void;
}) {
  return (
    <div className="feature-notice" role="status">
      <p>{message}</p>
      <button type="button" onClick={onDismiss}>
        Dismiss
      </button>
    </div>
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
