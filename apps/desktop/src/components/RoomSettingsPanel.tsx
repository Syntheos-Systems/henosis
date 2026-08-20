/** Read-only room context with authoritative manager bridge controls. */
import { useState } from "react";
import type { RoomBridgeStatus } from "../domain/agentControl";
import type { RoomStatus, RoomSummary } from "../domain/rooms";
import { normalizeClientError } from "../services/henosisClient";

/** Inputs for room metadata and existing bridge-control routes. */
export interface RoomSettingsPanelProps {
  /** Selected room's sanitized directory summary. */
  readonly room: RoomSummary;
  /** Latest public bridge lifecycle projection. */
  readonly status: RoomBridgeStatus;
  /** Authoritative room-management permission for the signed-in human. */
  readonly canManageRoom: boolean;
  /** Pause autonomous room bridge activity. */
  readonly onPause: () => Promise<void>;
  /** Resume autonomous room bridge activity. */
  readonly onResume: () => Promise<void>;
}

/** Human-facing directory connection state that does not imply bridge lifecycle. */
function roomStatusLabel(status: RoomStatus): string {
  switch (status) {
    case "quiet":
      return "Quiet room";
    case "active":
      return "Active room";
    case "paused":
      return "Paused room";
    case "disconnected":
      return "Disconnected room";
    case "awaiting-approval":
      return "Awaiting approval";
  }
}

/** Format one nullable bridge revision without fabricating revision zero. */
function revisionLabel(label: string, revision: number | null): string {
  return revision === null
    ? `No ${label.toLowerCase()} revision`
    : `${label} revision ${revision}`;
}

/** Render read-only room facts and permission-gated bridge pause or resume. */
export function RoomSettingsPanel({
  room,
  status,
  canManageRoom,
  onPause,
  onResume,
}: RoomSettingsPanelProps) {
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  /** Run one bridge mutation while preventing duplicate requests. */
  async function runBridgeAction(action: () => Promise<void>): Promise<void> {
    if (busy) {
      return;
    }
    setBusy(true);
    setActionError(null);
    try {
      await action();
    } catch (error) {
      setActionError(normalizeClientError(error).message);
    } finally {
      setBusy(false);
    }
  }

  const approvalLabel = `${room.pendingApprovals} pending ${
    room.pendingApprovals === 1 ? "approval" : "approvals"
  }`;

  return (
    <div className="room-settings-panel">
      <header className="dashboard-overview-heading">
        <div>
          <p className="eyebrow">Room</p>
          <h3>{room.name}</h3>
          <p>{room.topic ?? "No room topic has been set."}</p>
        </div>
        <span>{canManageRoom ? "Room manager" : "Member access"}</span>
      </header>

      <dl className="room-settings-panel__facts">
        <div>
          <dt>Server</dt>
          <dd>{room.serverName ?? room.serverId}</dd>
        </div>
        <div>
          <dt>Members</dt>
          <dd>{room.participants.length} visible members</dd>
        </div>
        <div>
          <dt>Approvals</dt>
          <dd>{approvalLabel}</dd>
        </div>
        <div>
          <dt>Connection</dt>
          <dd>{roomStatusLabel(room.status)}</dd>
        </div>
      </dl>

      <section
        className="bridge-control-card"
        aria-labelledby="bridge-control-title"
      >
        <header>
          <div>
            <p className="eyebrow">Room bridge</p>
            <h4 id="bridge-control-title">
              {status.paused ? "Bridge paused" : "Bridge running"}
            </h4>
          </div>
          <span className={`runtime-state runtime-state--${status.runtimeActivation}`}>
            {status.runtimeActivation}
          </span>
        </header>
        <dl>
          <div>
            <dt>Desired</dt>
            <dd>{revisionLabel("Desired", status.desiredRevision)}</dd>
          </div>
          <div>
            <dt>Active</dt>
            <dd>{revisionLabel("Active", status.activeRevision)}</dd>
          </div>
          <div>
            <dt>Last good</dt>
            <dd>{revisionLabel("Last good", status.lastGoodRevision)}</dd>
          </div>
        </dl>
        {status.runtimeErrorMessage ? (
          <p className="bridge-control-card__runtime-error">{status.runtimeErrorMessage}</p>
        ) : null}
        {actionError ? (
          <p className="dashboard-error" role="alert">
            {actionError}
          </p>
        ) : null}
        {canManageRoom ? (
          <button
            className="button button-secondary"
            type="button"
            disabled={busy}
            onClick={() => void runBridgeAction(status.paused ? onResume : onPause)}
          >
            {busy ? "Updating bridge…" : status.paused ? "Resume bridge" : "Pause bridge"}
          </button>
        ) : (
          <p className="dashboard-note">Bridge controls require room-manager access.</p>
        )}
      </section>

      <section className="dashboard-future-state" aria-label="Room metadata boundary">
        <p className="eyebrow">Read-only in this release</p>
        <h4>Room metadata</h4>
        <p>Room name, topic, and membership are shown here without mutation controls.</p>
      </section>
    </div>
  );
}
