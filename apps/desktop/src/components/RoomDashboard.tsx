/** Sanitized room-control loading and navigation shell. */
import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";
import type {
  AgentControlAction,
  AgentControlState,
  AgentControlValidationIssue,
  AgentIdentity,
  AgentRosterSnapshot,
  OwnedAgentIdentity,
  RoomBridgeStatus,
} from "../domain/agentControl";
import {
  applyAgentControlAction,
  createAgentControlState,
  serializeAgentControlDraft,
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
import { DashboardApplyBar } from "./DashboardApplyBar";
import { RoomPeoplePanel } from "./RoomPeoplePanel";
import { RoomSettingsPanel } from "./RoomSettingsPanel";

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

/** Local request and recovery feedback layered over the pure roster reducer. */
interface ApplyWorkflowState {
  /** Local validation failures from the latest attempted apply. */
  readonly validationIssues: readonly AgentControlValidationIssue[];
  /** Sanitized transport failure retained while the draft remains editable. */
  readonly errorMessage: string | null;
  /** Whether one atomic roster replacement is currently in flight. */
  readonly saving: boolean;
  /** Whether activation stayed pending through the bounded polling window. */
  readonly activationTimedOut: boolean;
}

/** Activation polling intervals with a ten-second maximum delay. */
const ACTIVATION_POLL_DELAYS_MS = [1_000, 2_000, 4_000, 8_000, 10_000] as const;

/** Maximum time spent polling one exact desired roster revision. */
const ACTIVATION_POLL_LIMIT_MS = 60_000;

/** Create isolated empty workflow feedback for one room dashboard generation. */
function createApplyWorkflowState(): ApplyWorkflowState {
  return {
    validationIssues: [],
    errorMessage: null,
    saving: false,
    activationTimedOut: false,
  };
}

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
  const bridgeMutationGeneration = useRef(0);
  const applyWorkflowGeneration = useRef(0);
  const [applyWorkflow, setApplyWorkflow] = useState<ApplyWorkflowState>(
    createApplyWorkflowState,
  );
  const [loadState, setLoadState] = useState<DashboardLoadState>(() =>
    currentUserId ? { status: "loading" } : { status: "disconnected" },
  );
  /** Exact server currently eligible for activation polling. */
  const pollingServerId =
    loadState.status === "ready" ? loadState.control.serverSnapshot.serverId : null;
  /** Exact desired revision currently eligible for activation polling. */
  const pollingDesiredRevision =
    loadState.status === "ready" ? loadState.control.bridgeStatus.desiredRevision : null;
  /** Runtime state used to start and stop the bounded polling loop. */
  const pollingActivation =
    loadState.status === "ready" ? loadState.control.bridgeStatus.runtimeActivation : null;

  /** Invalidate every in-flight bridge mutation when the room context changes. */
  useEffect(() => {
    bridgeMutationGeneration.current += 1;
    applyWorkflowGeneration.current += 1;
    setApplyWorkflow(createApplyWorkflowState());
    return () => {
      bridgeMutationGeneration.current += 1;
      applyWorkflowGeneration.current += 1;
    };
  }, [client, room.serverId]);

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

  /** Poll one exact pending desired revision with cancellation and a hard time limit. */
  useEffect(() => {
    if (
      pollingServerId === null ||
      pollingDesiredRevision === null ||
      pollingActivation !== "pending"
    ) {
      return;
    }
    const serverId = pollingServerId;
    const desiredRevision = pollingDesiredRevision;
    let cancelled = false;
    let elapsedMs = 0;
    let delayIndex = 0;
    let timeoutId: number | undefined;

    /** Project a same-revision status or adopt a newly discovered roster revision. */
    async function pollOnce(): Promise<void> {
      try {
        const status = await client.getRoomBridgeStatus(serverId);
        if (cancelled) {
          return;
        }
        if (status.desiredRevision !== desiredRevision) {
          const snapshot = await client.getRoomAgentRoster(serverId);
          if (cancelled) {
            return;
          }
          setLoadState((current) => {
            if (
              current.status !== "ready" ||
              current.control.serverSnapshot.serverId !== serverId ||
              current.control.bridgeStatus.desiredRevision !== desiredRevision
            ) {
              return current;
            }
            return {
              status: "ready",
              control: applyAgentControlAction(current.control, {
                type: current.control.dirty ? "revisionConflict" : "applySucceeded",
                snapshot,
              }),
            };
          });
          return;
        }
        setLoadState((current) => {
          if (
            current.status !== "ready" ||
            current.control.serverSnapshot.serverId !== serverId ||
            current.control.bridgeStatus.desiredRevision !== desiredRevision
          ) {
            return current;
          }
          return {
            status: "ready",
            control: applyAgentControlAction(current.control, {
              type: "runtimeUpdated",
              status,
            }),
          };
        });
        if (status.runtimeActivation === "pending") {
          if (elapsedMs >= ACTIVATION_POLL_LIMIT_MS) {
            setApplyWorkflow((current) => ({
              ...current,
              activationTimedOut: true,
            }));
          } else {
            scheduleNextPoll();
          }
        } else {
          setApplyWorkflow((current) => ({
            ...current,
            activationTimedOut: false,
          }));
        }
      } catch (error) {
        if (!cancelled) {
          setApplyWorkflow((current) => ({
            ...current,
            errorMessage: normalizeClientError(error).message,
            activationTimedOut: true,
          }));
        }
      }
    }

    /** Schedule the next capped interval without exceeding sixty seconds. */
    function scheduleNextPoll(): void {
      const candidateDelay =
        ACTIVATION_POLL_DELAYS_MS[
          Math.min(delayIndex, ACTIVATION_POLL_DELAYS_MS.length - 1)
        ];
      const delay = Math.min(
        candidateDelay,
        ACTIVATION_POLL_LIMIT_MS - elapsedMs,
      );
      delayIndex += 1;
      timeoutId = window.setTimeout(() => {
        elapsedMs += delay;
        void pollOnce();
      }, delay);
    }

    scheduleNextPoll();
    return () => {
      cancelled = true;
      if (timeoutId !== undefined) {
        window.clearTimeout(timeoutId);
      }
    };
  }, [client, pollingActivation, pollingDesiredRevision, pollingServerId]);

  /** Retry every read-context request as one consistent snapshot attempt. */
  function retry(): void {
    setRetryGeneration((generation) => generation + 1);
  }

  /** Apply one UI intent through the immutable agent-control reducer. */
  function dispatchControl(action: AgentControlAction): void {
    setApplyWorkflow((current) => ({
      ...current,
      validationIssues: [],
      errorMessage: null,
    }));
    setLoadState((current) =>
      current.status === "ready"
        ? {
            status: "ready",
            control: applyAgentControlAction(current.control, action),
          }
        : current,
    );
  }

  /** Test whether one async roster workflow still belongs to the selected room. */
  function workflowIsCurrent(serverId: string, generation: number): boolean {
    return (
      applyWorkflowGeneration.current === generation &&
      room.serverId === serverId
    );
  }

  /** Apply the complete validated roster with optimistic revision protection. */
  async function applyRoster(): Promise<void> {
    if (loadState.status !== "ready") {
      return;
    }
    const serialized = serializeAgentControlDraft(loadState.control);
    if (!serialized.ok) {
      setApplyWorkflow((current) => ({
        ...current,
        validationIssues: serialized.issues,
        errorMessage: null,
      }));
      return;
    }
    const serverId = loadState.control.serverSnapshot.serverId;
    const generation = ++applyWorkflowGeneration.current;
    setApplyWorkflow((current) => ({
      ...current,
      validationIssues: [],
      errorMessage: null,
      saving: true,
      activationTimedOut: false,
    }));
    try {
      const snapshot = await client.applyRoomAgentRoster(
        serverId,
        serialized.request,
      );
      if (!workflowIsCurrent(serverId, generation)) {
        return;
      }
      setLoadState((current) =>
        current.status === "ready" &&
        current.control.serverSnapshot.serverId === serverId
          ? {
              status: "ready",
              control: applyAgentControlAction(current.control, {
                type: "applySucceeded",
                snapshot,
              }),
            }
          : current,
      );
    } catch (error) {
      const normalized = normalizeClientError(error);
      if (
        normalized.kind === "conflict" &&
        normalized.code === "revision_conflict"
      ) {
        try {
          const snapshot = await client.getRoomAgentRoster(serverId);
          if (!workflowIsCurrent(serverId, generation)) {
            return;
          }
          setLoadState((current) =>
            current.status === "ready" &&
            current.control.serverSnapshot.serverId === serverId
              ? {
                  status: "ready",
                  control: applyAgentControlAction(current.control, {
                    type: "revisionConflict",
                    snapshot,
                  }),
                }
              : current,
          );
        } catch (refreshError) {
          if (workflowIsCurrent(serverId, generation)) {
            setApplyWorkflow((current) => ({
              ...current,
              errorMessage: normalizeClientError(refreshError).message,
            }));
          }
        }
      } else if (workflowIsCurrent(serverId, generation)) {
        setApplyWorkflow((current) => ({
          ...current,
          errorMessage: normalized.message,
        }));
      }
    } finally {
      if (workflowIsCurrent(serverId, generation)) {
        setApplyWorkflow((current) => ({ ...current, saving: false }));
      }
    }
  }

  /** Restore current server truth and invalidate any in-flight roster mutation. */
  function discardRoster(): void {
    applyWorkflowGeneration.current += 1;
    setApplyWorkflow(createApplyWorkflowState());
    dispatchControl({ type: "discard" });
  }

  /** Retry activation of the current desired revision without applying a new one. */
  async function retryActivation(): Promise<void> {
    if (loadState.status !== "ready") {
      return;
    }
    const serverId = loadState.control.serverSnapshot.serverId;
    const generation = ++applyWorkflowGeneration.current;
    setApplyWorkflow((current) => ({
      ...current,
      errorMessage: null,
      saving: true,
      activationTimedOut: false,
    }));
    try {
      const snapshot = await client.reconcileRoomBridge(serverId);
      if (!workflowIsCurrent(serverId, generation)) {
        return;
      }
      setLoadState((current) =>
        current.status === "ready" &&
        current.control.serverSnapshot.serverId === serverId
          ? {
              status: "ready",
              control: applyAgentControlAction(current.control, {
                type: "applySucceeded",
                snapshot,
              }),
            }
          : current,
      );
    } catch (error) {
      if (workflowIsCurrent(serverId, generation)) {
        setApplyWorkflow((current) => ({
          ...current,
          errorMessage: normalizeClientError(error).message,
        }));
      }
    } finally {
      if (workflowIsCurrent(serverId, generation)) {
        setApplyWorkflow((current) => ({ ...current, saving: false }));
      }
    }
  }

  /** Refresh bridge status once after bounded polling stops. */
  async function refreshActivation(): Promise<void> {
    if (loadState.status !== "ready") {
      return;
    }
    const serverId = loadState.control.serverSnapshot.serverId;
    const desiredRevision = loadState.control.bridgeStatus.desiredRevision;
    const generation = ++applyWorkflowGeneration.current;
    setApplyWorkflow((current) => ({
      ...current,
      errorMessage: null,
      saving: true,
    }));
    try {
      const status = await client.getRoomBridgeStatus(serverId);
      if (!workflowIsCurrent(serverId, generation)) {
        return;
      }
      if (status.desiredRevision !== desiredRevision) {
        const snapshot = await client.getRoomAgentRoster(serverId);
        if (!workflowIsCurrent(serverId, generation)) {
          return;
        }
        setLoadState((current) => {
          if (
            current.status !== "ready" ||
            current.control.serverSnapshot.serverId !== serverId
          ) {
            return current;
          }
          return {
            status: "ready",
            control: applyAgentControlAction(current.control, {
              type: current.control.dirty ? "revisionConflict" : "applySucceeded",
              snapshot,
            }),
          };
        });
      } else {
        setLoadState((current) =>
          current.status === "ready" &&
          current.control.serverSnapshot.serverId === serverId
            ? {
                status: "ready",
                control: applyAgentControlAction(current.control, {
                  type: "runtimeUpdated",
                  status,
                }),
              }
            : current,
        );
      }
      setApplyWorkflow((current) => ({
        ...current,
        activationTimedOut: status.runtimeActivation === "pending",
      }));
    } catch (error) {
      if (workflowIsCurrent(serverId, generation)) {
        setApplyWorkflow((current) => ({
          ...current,
          errorMessage: normalizeClientError(error).message,
          activationTimedOut: true,
        }));
      }
    } finally {
      if (workflowIsCurrent(serverId, generation)) {
        setApplyWorkflow((current) => ({ ...current, saving: false }));
      }
    }
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

  /** Apply one authoritative bridge result without overwriting a changed room. */
  function applyBridgeStatus(
    serverId: string,
    generation: number,
    status: RoomBridgeStatus,
  ): void {
    setLoadState((current) => {
      if (
        bridgeMutationGeneration.current !== generation ||
        current.status !== "ready" ||
        current.control.serverSnapshot.serverId !== serverId
      ) {
        return current;
      }
      return {
        status: "ready",
        control: applyAgentControlAction(current.control, {
          type: "runtimeUpdated",
          status,
        }),
      };
    });
  }

  /** Pause the selected room bridge and retain the server's returned truth. */
  async function pauseBridge(): Promise<void> {
    const serverId = room.serverId;
    const generation = ++bridgeMutationGeneration.current;
    const status = await client.pauseRoomBridge(serverId);
    applyBridgeStatus(serverId, generation, status);
  }

  /** Resume the selected room bridge and retain the server's returned truth. */
  async function resumeBridge(): Promise<void> {
    const serverId = room.serverId;
    const generation = ++bridgeMutationGeneration.current;
    const status = await client.resumeRoomBridge(serverId);
    applyBridgeStatus(serverId, generation, status);
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
      discardRoster();
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
        onPauseBridge={pauseBridge}
        onResumeBridge={resumeBridge}
        validationIssues={applyWorkflow.validationIssues}
        editingDisabled={applyWorkflow.saving}
      />
      {activeTab === "agents" ? (
        <DashboardApplyBar
          control={loadState.control}
          validationIssues={applyWorkflow.validationIssues}
          errorMessage={applyWorkflow.errorMessage}
          saving={applyWorkflow.saving}
          activationTimedOut={applyWorkflow.activationTimedOut}
          onApply={() => void applyRoster()}
          onDiscard={discardRoster}
          onRetryActivation={() => void retryActivation()}
          onRefreshActivation={() => void refreshActivation()}
        />
      ) : null}
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
  /** Pause the authoritative bridge for the selected room. */
  readonly onPauseBridge: () => Promise<void>;
  /** Resume the authoritative bridge for the selected room. */
  readonly onResumeBridge: () => Promise<void>;
  /** Latest local validation failures attached to roster or seat controls. */
  readonly validationIssues: readonly AgentControlValidationIssue[];
  /** Whether roster controls must remain stable during one mutation. */
  readonly editingDisabled: boolean;
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
  onPauseBridge,
  onResumeBridge,
  validationIssues,
  editingDisabled,
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
          validationIssues={validationIssues}
          editingDisabled={editingDisabled}
        />
      ) : null}
      {tab === "people" ? (
        <RoomPeoplePanel
          room={room}
          currentUserId={control.currentHumanId}
          identities={control.identities}
        />
      ) : null}
      {tab === "room" ? (
        <RoomSettingsPanel
          key={room.serverId}
          room={room}
          status={control.bridgeStatus}
          canManageRoom={control.canManageRoom}
          onPause={onPauseBridge}
          onResume={onResumeBridge}
        />
      ) : null}
    </section>
  );
}
