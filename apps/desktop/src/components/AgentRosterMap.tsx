/** Vertical room topology for persistent agent identities and seats. */
import type {
  AgentControlAction,
  AgentControlState,
  AgentIdentity,
} from "../domain/agentControl";
import { AgentSeatCard } from "./AgentSeatCard";

/** Inputs for the room's complete agent roster editor. */
export interface AgentRosterMapProps {
  /** Complete immutable room-agent reducer state. */
  readonly control: AgentControlState;
  /** Send one user intent through the authoritative reducer. */
  readonly onAction: (action: AgentControlAction) => void;
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
function availableOwnedIdentities(control: AgentControlState): AgentIdentity[] {
  const seated = new Set(control.draft.map((seat) => seat.agentIdentityId));
  return control.identities.filter(
    (identity) =>
      identity.ownerUserId === control.currentHumanId && !seated.has(identity.id),
  );
}

/** Render the room roster as an ordered topology with explicit add affordances. */
export function AgentRosterMap({ control, onAction }: AgentRosterMapProps) {
  const available = availableOwnedIdentities(control);
  const seatCount = control.draft.length;

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
          <p className="eyebrow">Owned identities</p>
          <h4 id="available-agent-title">Add to this room</h4>
        </div>
        {available.length === 0 ? (
          <p>Every identity you own is already in this room.</p>
        ) : (
          <div className="agent-roster-map__add-actions">
            {available.map((identity) => {
              const name = identity.displayName?.trim() || identity.username;
              return (
                <button
                  key={identity.id}
                  className="button button-secondary"
                  type="button"
                  aria-label={`Add ${name} to room`}
                  onClick={() =>
                    onAction({
                      type: "addSeat",
                      seatId: createDraftSeatId(identity.id),
                      agentIdentityId: identity.id,
                    })
                  }
                >
                  Add {name}
                </button>
              );
            })}
          </div>
        )}
      </section>
    </div>
  );
}
