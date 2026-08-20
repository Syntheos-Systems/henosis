/** Read-only room-human and persistent-agent ownership topology. */
import type {
  AgentIdentity,
  OwnedAgentIdentity,
  UnownedAgentIdentity,
} from "../domain/agentControl";
import type { RoomParticipant, RoomSummary } from "../domain/rooms";

/** Inputs for the truthful People dashboard projection. */
export interface RoomPeoplePanelProps {
  /** Selected room containing visible human presentation data. */
  readonly room: RoomSummary;
  /** Stable authenticated Rift human identifier. */
  readonly currentUserId: string;
  /** Owned and roster-visible persistent identities known to the dashboard. */
  readonly identities: readonly AgentIdentity[];
}

/** One stable human owner node and the persistent agents attached to it. */
interface HumanOwnerProjection {
  /** Stable human identifier used for exact ownership joins. */
  readonly id: string;
  /** Truthful human-facing label without display-name inference. */
  readonly label: string;
  /** Current-human or read-only relationship description. */
  readonly relationship: string;
  /** Optional room presence supplied for the exact stable ID. */
  readonly presence: string | null;
  /** Persistent agent identities whose owner ID exactly matches this node. */
  readonly agents: readonly OwnedAgentIdentity[];
}

/** Resolve a participant only through an exact stable human ID match. */
function humanParticipant(
  room: RoomSummary,
  humanId: string,
): RoomParticipant | undefined {
  return room.participants.find(
    (participant) => !participant.isAgent && participant.id === humanId,
  );
}

/** Build deterministic human owner nodes without joining by display name. */
function ownerProjections(
  room: RoomSummary,
  currentUserId: string,
  identities: readonly AgentIdentity[],
): HumanOwnerProjection[] {
  const owned = identities.filter(
    (identity): identity is OwnedAgentIdentity => identity.ownerUserId !== null,
  );
  const ownerIds = new Set(owned.map((identity) => identity.ownerUserId));
  const participantIds = room.participants
    .filter((participant) => !participant.isAgent)
    .map((participant) => participant.id);
  const orderedIds = [
    currentUserId,
    ...participantIds.filter((id) => id !== currentUserId),
    ...[...ownerIds].filter(
      (id) => id !== currentUserId && !participantIds.includes(id),
    ),
  ];
  return [...new Set(orderedIds)].map((id) => {
    const participant = humanParticipant(room, id);
    const current = id === currentUserId;
    return {
      id,
      label: current ? "You" : participant?.displayName || "Another member",
      relationship: current
        ? participant?.displayName || "Signed-in human"
        : "Read-only member",
      presence: participant?.presence ?? null,
      agents: owned.filter((identity) => identity.ownerUserId === id),
    };
  });
}

/** Resolve imported identities whose explicit owner remains absent. */
function unownedIdentities(
  identities: readonly AgentIdentity[],
): UnownedAgentIdentity[] {
  return identities.filter(
    (identity): identity is UnownedAgentIdentity => identity.ownerUserId === null,
  );
}

/** Resolve the public label for one persistent agent identity. */
function agentName(identity: AgentIdentity): string {
  return identity.displayName?.trim() || identity.username;
}

/** Render a read-only ownership map and explicit invitation release boundary. */
export function RoomPeoplePanel({
  room,
  currentUserId,
  identities,
}: RoomPeoplePanelProps) {
  const owners = ownerProjections(room, currentUserId, identities);
  const unowned = unownedIdentities(identities);

  return (
    <div className="room-people-panel">
      <header className="dashboard-overview-heading">
        <div>
          <p className="eyebrow">People</p>
          <h3>People and ownership</h3>
          <p>Human identity anchors persistent agents across rooms.</p>
        </div>
        <span>
          {room.participants.filter((participant) => !participant.isAgent).length}{" "}
          room-visible humans
        </span>
      </header>

      <div className="room-people-panel__owners">
        {owners.map((owner) => (
          <section
            className="human-owner-card"
            role="group"
            aria-label={owner.label}
            key={owner.id}
          >
            <header>
              <div>
                <h4>{owner.label}</h4>
                <p>{owner.relationship}</p>
              </div>
              {owner.presence ? (
                <span className="human-owner-card__presence">{owner.presence}</span>
              ) : null}
            </header>
            {owner.agents.length === 0 ? (
              <p className="human-owner-card__empty">
                No persistent agents in this dashboard context.
              </p>
            ) : (
              <ul aria-label={`${owner.label} persistent agents`}>
                {owner.agents.map((identity) => (
                  <li key={identity.id}>
                    <strong>{agentName(identity)}</strong>
                    <span>@{identity.username}</span>
                  </li>
                ))}
              </ul>
            )}
          </section>
        ))}
      </div>

      <section
        className="unowned-identities-card"
        role="group"
        aria-label="Needs an owner"
      >
        <div>
          <p className="eyebrow">Imported identities</p>
          <h4>Needs an owner</h4>
        </div>
        {unowned.length === 0 ? (
          <p>No roster-visible identities are waiting for an owner.</p>
        ) : (
          <ul>
            {unowned.map((identity) => (
              <li key={identity.id}>
                <strong>{agentName(identity)}</strong>
                <span>@{identity.username}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <section className="dashboard-future-state" aria-label="Future invitation state">
        <p className="eyebrow">Future release</p>
        <h4>Human invitations</h4>
        <p>Invitations are not part of this release.</p>
      </section>
    </div>
  );
}
