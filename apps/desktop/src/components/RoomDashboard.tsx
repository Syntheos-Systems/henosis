/** Sanitized room-control loading and navigation shell. */
import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import type {
  AgentControlAction,
  AgentControlState,
  AgentIdentity,
  AgentRosterSnapshot,
  OwnedAgentIdentity,
} from "../domain/agentControl";
import {
  applyAgentControlAction,
  createAgentControlState,
} from "../domain/agentControl";
import type { RoomSummary } from "../domain/rooms";
import type {
  HenosisClient,
  HenosisClientError,
} from "../services/henosisClient";
import { normalizeClientError } from "../services/henosisClient";
import {
  DashboardTabs,
  dashboardPanelId,
  dashboardTabId,
} from "./DashboardTabs";
import type { DashboardTabId } from "./DashboardTabs";
import { AgentRosterMap } from "./AgentRosterMap";

/** Inputs required to load one room's control context. */
export interface RoomDashboardProps {
  /** Shared sanitized native or fixture adapter. */
  readonly client: HenosisClient;
  /** Selected room supplying distinct room and server identifiers. */
  readonly room: RoomSummary;
  /** Explicit authenticated human, absent for a disconnected cache. */
  readonly currentUserId: string | undefined;
  /** Reveal connection setup when live dashboard context is unavailable. */
  readonly onReconnect: () => void;
  /** Publish reducer dirtiness to the room-level navigation guard. */
  readonly onDirtyChange?: (dirty: boolean) => void;
}

/** Dashboard state before or during one complete read-context load. */
interface LoadingDashboardState {
  /** Loading-state discriminator. */
  readonly status: "loading";
}

/** Dashboard state when no authenticated human can authorize reads. */
interface DisconnectedDashboardState {
  /** Disconnected-state discriminator. */
  readonly status: "disconnected";
}

/** Dashboard state after a bounded client failure. */
interface FailedDashboardState {
  /** Failure-state discriminator. */
  readonly status: "failed";
  /** Sanitized actionable client error. */
  readonly error: HenosisClientError;
}

/** Dashboard state after every read-context request succeeds. */
interface ReadyDashboardState {
  /** Ready-state discriminator. */
  readonly status: "ready";
  /** Complete immutable reducer state. */
  readonly control: AgentControlState;
}

/** Complete dashboard load-state union. */
type DashboardLoadState =
  | LoadingDashboardState
  | DisconnectedDashboardState
  | FailedDashboardState
  | ReadyDashboardState;

/** Compose owned and roster-visible identities without duplicate stable IDs. */
export function composeDashboardIdentities(
  owned: readonly OwnedAgentIdentity[],
  roster: AgentRosterSnapshot,
): AgentIdentity[] {
  const identities = new Map<string, AgentIdentity>(
    owned.map((identity) => [identity.id, { ...identity }]),
  );
  roster.seats.forEach((seat) => {
    if (!identities.has(seat.agentIdentityId)) {
      identities.set(seat.agentIdentityId, {
        id: seat.agentIdentityId,
        username: seat.agentUsername,
        displayName: seat.agentDisplayName,
        ownerUserId: seat.ownerHumanId,
      });
    }
  });
  return [...identities.values()];
}

/** Render and load the room dashboard without exposing native secrets. */
export function RoomDashboard({
  client,
  room,
  currentUserId,
  onReconnect,
  onDirtyChange = () => undefined,
}: RoomDashboardProps) {
  const [activeTab, setActiveTab] = useState<DashboardTabId>("agents");
  const [retryGeneration, setRetryGeneration] = useState(0);
  const [loadState, setLoadState] = useState<DashboardLoadState>(() =>
    currentUserId ? { status: "loading" } : { status: "disconnected" },
  );

  useEffect(() => {
    if (!currentUserId) {
      setLoadState({ status: "disconnected" });
      return;
    }
    const authenticatedUserId = currentUserId;
    let active = true;
    setLoadState({ status: "loading" });

    /** Load every independent dashboard read before constructing reducer state. */
    async function loadDashboard(): Promise<void> {
      try {
        const [owned, permissions, catalog, roster, bridgeStatus] =
          await Promise.all([
            client.getMyAgents(),
            client.getRoomPermissions(room.serverId),
            client.getAgentCapabilities(room.serverId),
            client.getRoomAgentRoster(room.serverId),
            client.getRoomBridgeStatus(room.serverId),
          ]);
        if (!active) {
          return;
        }
        const identities = composeDashboardIdentities(owned, roster);
        const initialized = createAgentControlState({
          currentHumanId: authenticatedUserId,
          canManageRoom: permissions.manageServer,
          identities,
          catalog,
          snapshot: roster,
        });
        const control = applyAgentControlAction(initialized, {
          type: "runtimeUpdated",
          status: bridgeStatus,
        });
        setLoadState({ status: "ready", control });
      } catch (error) {
        if (active) {
          setLoadState({
            status: "failed",
            error: normalizeClientError(error),
          });
        }
      }
    }

    void loadDashboard();
    return () => {
      active = false;
    };
  }, [client, currentUserId, retryGeneration, room.serverId]);

  useEffect(() => {
    onDirtyChange(loadState.status === "ready" && loadState.control.dirty);
  }, [loadState, onDirtyChange]);

  /** Retry every read-context request as one consistent snapshot attempt. */
  function retry(): void {
    setRetryGeneration((generation) => generation + 1);
  }

  /** Apply one UI intent through the immutable agent-control reducer. */
  function dispatchControl(action: AgentControlAction): void {
    setLoadState((current) =>
      current.status === "ready"
        ? {
            status: "ready",
            control: applyAgentControlAction(current.control, action),
          }
        : current,
    );
  }

  /** Create one persistent identity through the authenticated client boundary. */
  async function createIdentity(
    username: string,
    displayName: string | null,
  ): Promise<OwnedAgentIdentity> {
    return client.createMyAgent(username, displayName);
  }

  /** Claim one roster-visible imported identity through the client boundary. */
  async function claimIdentity(agentIdentityId: string): Promise<OwnedAgentIdentity> {
    return client.claimAgent(agentIdentityId);
  }

  /** Refresh mutation-dependent identity truth and preserve any unsaved room draft. */
  async function refreshIdentityContext(
    identity: OwnedAgentIdentity,
    addToRoom: boolean,
  ): Promise<void> {
    const serverId = room.serverId;
    const [owned, roster] = await Promise.all([
      client.getMyAgents(),
      client.getRoomAgentRoster(serverId),
    ]);
    const identities = composeDashboardIdentities(owned, roster);
    setLoadState((current) => {
      if (
        current.status !== "ready" ||
        current.control.serverSnapshot.serverId !== serverId
      ) {
        return current;
      }
      let control = applyAgentControlAction(current.control, {
        type: "identityContextUpdated",
        identities,
        snapshot: roster,
      });
      if (addToRoom) {
        control = applyAgentControlAction(control, {
          type: "addSeat",
          seatId: `draft-${identity.id}-${Date.now().toString(36)}`,
          agentIdentityId: identity.id,
        });
      }
      return { status: "ready", control };
    });
  }

  /** Guard leaving Agents when roster controls contain a local draft. */
  function canSelectTab(nextTab: DashboardTabId): boolean {
    if (
      nextTab === activeTab ||
      activeTab !== "agents" ||
      loadState.status !== "ready" ||
      !loadState.control.dirty
    ) {
      return true;
    }
    const discard = window.confirm(
      "Discard unsaved agent changes and leave the Agents tab? Choose Cancel to remain.",
    );
    if (discard) {
      setLoadState({
        status: "ready",
        control: applyAgentControlAction(loadState.control, { type: "discard" }),
      });
    }
    return discard;
  }

  if (loadState.status === "disconnected") {
    return (
      <DashboardState className="dashboard-state--unavailable">
        <p className="eyebrow">Room dashboard</p>
        <h2>Reconnect to manage this room</h2>
        <p>Cached room context cannot identify or authorize the current human.</p>
        <button className="button button-secondary" type="button" onClick={onReconnect}>
          Reconnect
        </button>
      </DashboardState>
    );
  }

  if (loadState.status === "loading") {
    return (
      <DashboardState className="dashboard-state--loading" role="status">
        <p className="eyebrow">Room dashboard</p>
        <h2>Loading room controls</h2>
        <p>Reading identities, permissions, capabilities, roster, and bridge state.</p>
      </DashboardState>
    );
  }

  if (loadState.status === "failed") {
    return (
      <DashboardState className="dashboard-state--unavailable" role="alert">
        <p className="eyebrow">Room dashboard</p>
        <h2>Room controls unavailable</h2>
        <p>{loadState.error.message}</p>
        <div className="dashboard-state__actions">
          <button className="button button-primary" type="button" onClick={retry}>
            Retry room controls
          </button>
          <button className="button button-secondary" type="button" onClick={onReconnect}>
            Reconnect
          </button>
        </div>
      </DashboardState>
    );
  }

  return (
    <div className="room-dashboard-content">
      <header className="room-dashboard-heading">
        <div>
          <p className="eyebrow">Room dashboard</p>
          <h2>Room controls</h2>
        </div>
        <span className="dashboard-access">
          {loadState.control.canManageRoom ? "Room manager" : "Member access"}
        </span>
      </header>

      <DashboardTabs
        activeTab={activeTab}
        onSelect={setActiveTab}
        canSelect={canSelectTab}
      />
      <DashboardPanel
        tab={activeTab}
        control={loadState.control}
        room={room}
        onControlAction={dispatchControl}
        onCreateIdentity={createIdentity}
        onClaimIdentity={claimIdentity}
        onIdentityMutated={refreshIdentityContext}
      />
    </div>
  );
}

/** Props shared by compact dashboard loading and recovery states. */
interface DashboardStateProps {
  /** Optional state-specific styling. */
  readonly className?: string;
  /** Optional live-region role. */
  readonly role?: "status" | "alert";
  /** State content. */
  readonly children: ReactNode;
}

/** Render one vertically centered dashboard state. */
function DashboardState({
  className = "",
  role,
  children,
}: DashboardStateProps) {
  return (
    <section className={`dashboard-state ${className}`} role={role}>
      {children}
    </section>
  );
}

/** Inputs for the currently selected dashboard panel. */
interface DashboardPanelProps {
  /** Selected dashboard tab. */
  readonly tab: DashboardTabId;
  /** Loaded agent-control state. */
  readonly control: AgentControlState;
  /** Selected room context. */
  readonly room: RoomSummary;
  /** Send one roster intent through the room-level reducer state. */
  readonly onControlAction: (action: AgentControlAction) => void;
  /** Create one persistent identity for the current human. */
  readonly onCreateIdentity: (
    username: string,
    displayName: string | null,
  ) => Promise<OwnedAgentIdentity>;
  /** Claim one roster-visible imported identity. */
  readonly onClaimIdentity: (
    agentIdentityId: string,
  ) => Promise<OwnedAgentIdentity>;
  /** Refresh identity and roster truth after a successful mutation. */
  readonly onIdentityMutated: (
    identity: OwnedAgentIdentity,
    addToRoom: boolean,
  ) => Promise<void>;
}

/** Render the selected room-control panel. */
function DashboardPanel({
  tab,
  control,
  room,
  onControlAction,
  onCreateIdentity,
  onClaimIdentity,
  onIdentityMutated,
}: DashboardPanelProps) {
  return (
    <section
      className="dashboard-panel"
      id={dashboardPanelId(tab)}
      role="tabpanel"
      aria-labelledby={dashboardTabId(tab)}
      tabIndex={0}
    >
      {tab === "agents" ? (
        <AgentRosterMap
          control={control}
          onAction={onControlAction}
          onCreateIdentity={onCreateIdentity}
          onClaimIdentity={onClaimIdentity}
          onIdentityMutated={onIdentityMutated}
        />
      ) : null}
      {tab === "people" ? <PeopleOverview room={room} /> : null}
      {tab === "room" ? <RoomOverview room={room} /> : null}
    </section>
  );
}

/** Inputs for the initial people overview. */
interface PeopleOverviewProps {
  /** Selected room summary. */
  readonly room: RoomSummary;
}

/** Render the current participant summary. */
function PeopleOverview({ room }: PeopleOverviewProps) {
  return (
    <div className="dashboard-overview">
      <div>
        <p className="eyebrow">People</p>
        <h3>Room participants</h3>
      </div>
      <p>{`${room.participants.length} visible ${
        room.participants.length === 1 ? "participant" : "participants"
      }`}</p>
    </div>
  );
}

/** Inputs for the initial room overview. */
interface RoomOverviewProps {
  /** Selected room summary. */
  readonly room: RoomSummary;
}

/** Render sanitized room context and conversation state. */
function RoomOverview({ room }: RoomOverviewProps) {
  return (
    <div className="dashboard-overview">
      <div>
        <p className="eyebrow">Room</p>
        <h3>{room.topic ?? "Persistent conversation"}</h3>
      </div>
      <dl className="dashboard-facts">
        <div>
          <dt>Current thread</dt>
          <dd>{room.activeWork ?? "Open conversation"}</dd>
        </div>
        <div>
          <dt>Unread</dt>
          <dd>{room.unreadCount} messages</dd>
        </div>
      </dl>
    </div>
  );
}
