/** Shared room-presentational helpers for cards, rows, and room detail. */
import type { RoomParticipant, RoomStatus } from "../domain/rooms";

/** Human-readable labels for every room state. */
const STATUS_LABELS: Record<RoomStatus, string> = {
  quiet: "Quiet",
  active: "Active now",
  paused: "Bridge paused",
  disconnected: "Unavailable",
  "awaiting-approval": "Awaiting approval",
};

/** Render a compact relative timestamp for room activity. */
export function formatRelativeTime(isoTimestamp: string, now = new Date()): string {
  const activity = Date.parse(isoTimestamp);
  if (Number.isNaN(activity)) {
    return "Unknown";
  }

  const elapsedMinutes = Math.max(0, Math.round((now.getTime() - activity) / 60_000));
  if (elapsedMinutes < 1) {
    return "Now";
  }
  if (elapsedMinutes < 60) {
    return `${elapsedMinutes}m`;
  }
  const hours = Math.floor(elapsedMinutes / 60);
  if (hours < 24) {
    return `${hours}h`;
  }
  const days = Math.floor(hours / 24);
  return days < 7 ? `${days}d` : new Date(activity).toLocaleDateString();
}

/** Return the visible label for one room status. */
export function roomStatusLabel(status: RoomStatus): string {
  return STATUS_LABELS[status];
}

/** Derive a one or two-letter avatar fallback from a display name. */
function initials(displayName: string): string {
  return displayName
    .trim()
    .split(/\s+/)
    .slice(0, 2)
    .map((part) => part[0]?.toLocaleUpperCase())
    .join("");
}

/** Inputs for a compact room participant stack. */
export interface ParticipantStackProps {
  /** Participants to show before collapsing overflow. */
  participants: readonly RoomParticipant[];
  /** Maximum individually rendered participants. */
  limit?: number;
}

/** Render accessible initial-based avatars without trusting remote image content. */
export function ParticipantStack({
  participants,
  limit = 4,
}: ParticipantStackProps) {
  const visible = participants.slice(0, limit);
  const overflow = participants.length - visible.length;

  if (participants.length === 0) {
    return <span className="participant-empty">No one present</span>;
  }

  return (
    <div
      className="participant-stack"
      aria-label={participants.map((participant) => participant.displayName).join(", ")}
    >
      {visible.map((participant, index) => (
        <span
          className="participant-avatar"
          data-agent={participant.isAgent}
          style={{ zIndex: visible.length - index }}
          title={`${participant.displayName}${participant.isAgent ? " · agent" : ""}`}
          key={participant.id}
        >
          {initials(participant.displayName)}
        </span>
      ))}
      {overflow > 0 ? (
        <span className="participant-avatar participant-overflow">+{overflow}</span>
      ) : null}
    </div>
  );
}
