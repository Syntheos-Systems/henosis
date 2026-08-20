/** Vertical room topology for persistent agent identities and seats. */
import { useRef, useState } from "react";
import type {
  AgentControlAction,
  AgentControlState,
  OwnedAgentIdentity,
  UnownedAgentIdentity,
} from "../domain/agentControl";
import { AgentIdentityDialog } from "./AgentIdentityDialog";
import { AgentSeatCard } from "./AgentSeatCard";

/** Inputs for the room's complete agent roster editor. */
export interface AgentRosterMapProps {
  /** Complete immutable room-agent reducer state. */
  readonly control: AgentControlState;
  /** Send one user intent through the authoritative reducer. */
  readonly onAction: (action: AgentControlAction) => void;
  /** Create one server-owned identity for the signed-in human. */
  readonly onCreateIdentity: (
    username: string,
    displayName: string | null,
  ) => Promise<OwnedAgentIdentity>;
  /** Claim one roster-visible imported identity under manager authority. */
  readonly onClaimIdentity: (
    agentIdentityId: string,
  ) => Promise<OwnedAgentIdentity>;
  /** Refresh dashboard identity context after one successful mutation. */
  readonly onIdentityMutated: (
    identity: OwnedAgentIdentity,
    addToRoom: boolean,
  ) => Promise<void>;
}

/** Return a collision-resistant local seat identifier without server authority. */
function createDraftSeatId(identityId: string): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `draft-${identityId}-${Date.now().toString(36)}`;
}

/** Describe public bridge lifecycle state without implying activation success. */
function bridgeLabel(control: AgentControlState): string {
  if (control.bridgeStatus.paused) {
    return "Bridge paused";
  }
  return `Bridge ${control.bridgeStatus.runtimeActivation}`;
}

/** Resolve identities owned by the current human but absent from this room. */
function availableOwnedIdentities(control: AgentControlState): OwnedAgentIdentity[] {
  const seated = new Set(control.draft.map((seat) => seat.agentIdentityId));
  return control.identities.filter(
    (identity): identity is OwnedAgentIdentity =>
      identity.ownerUserId === control.currentHumanId && !seated.has(identity.id),
  );
}

/** Resolve unowned identities exposed by the selected room roster only. */
function visibleUnownedIdentities(control: AgentControlState): UnownedAgentIdentity[] {
  const rosterIdentityIds = new Set(
    control.serverSnapshot.seats.map((seat) => seat.agentIdentityId),
  );
  return control.identities.filter(
    (identity): identity is UnownedAgentIdentity =>
      identity.ownerUserId === null && rosterIdentityIds.has(identity.id),
  );
}

/** Render the room roster as an ordered topology with explicit add affordances. */
export function AgentRosterMap({
  control,
  onAction,
  onCreateIdentity,
  onClaimIdentity,
  onIdentityMutated,
}: AgentRosterMapProps) {
  const [identityDialogOpen, setIdentityDialogOpen] = useState(false);
  const identityTriggerRef = useRef<HTMLButtonElement>(null);
  const available = availableOwnedIdentities(control);
  const unowned = visibleUnownedIdentities(control);
  const seatCount = control.draft.length;

  /** Add one existing owned identity to the local room draft. */
  function selectIdentity(identityId: string): void {
    onAction({
      type: "addSeat",
      seatId: createDraftSeatId(identityId),
      agentIdentityId: identityId,
    });
  }

  return (
    <div className="agent-roster-map">
      <header className="agent-roster-map__header">
        <div>
          <p className="eyebrow">Agents</p>
          <h3>Agent topology</h3>
          <p>{`${seatCount} ${seatCount === 1 ? "agent" : "agents"} in this room`}</p>
        </div>
        <dl className="agent-roster-map__facts">
          <div>
            <dt>Available</dt>
            <dd>{control.identities.length} identities available</dd>
          </div>
          <div>
            <dt>Runtime</dt>
            <dd>{bridgeLabel(control)}</dd>
          </div>
        </dl>
      </header>

      {seatCount === 0 ? (
        <p className="dashboard-empty">No agents in this room yet.</p>
      ) : (
        <ol className="agent-roster-map__list" aria-label="Room agent seats">
          {control.draft.map((seat) => (
            <li key={seat.seatId}>
              <AgentSeatCard control={control} seat={seat} onAction={onAction} />
            </li>
          ))}
        </ol>
      )}

      <section className="agent-roster-map__add" aria-labelledby="available-agent-title">
        <div>
          <p className="eyebrow">Persistent identities</p>
          <h4 id="available-agent-title">Add to this room</h4>
          <p>Create an identity, choose one you own, or claim a visible import.</p>
        </div>
        <button
          ref={identityTriggerRef}
          className="button button-secondary"
          type="button"
          onClick={() => setIdentityDialogOpen(true)}
        >
          Add agent identity
        </button>
      </section>

      {identityDialogOpen ? (
        <AgentIdentityDialog
          ownedIdentities={available}
          unownedIdentities={unowned}
          canClaim={control.canManageRoom}
          onSelectIdentity={selectIdentity}
          onCreateIdentity={onCreateIdentity}
          onClaimIdentity={onClaimIdentity}
          onIdentityMutated={onIdentityMutated}
          onClose={() => setIdentityDialogOpen(false)}
          returnFocusRef={identityTriggerRef}
        />
      ) : null}
    </div>
  );
}
