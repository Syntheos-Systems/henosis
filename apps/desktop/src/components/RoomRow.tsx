/** Compact historical room entry with context and keyboard-safe actions. */
import {
  BellDot,
  CirclePause,
  ListTodo,
  MoreHorizontal,
} from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import {
  formatRelativeTime,
  ParticipantStack,
  roomStatusLabel,
} from "./roomPresentation";

/** Inputs and actions for one historical room row. */
export interface RoomRowProps {
  /** Room summary rendered in the history list. */
  room: RoomSummary;
  /** Enter the selected room. */
  onOpen(room: RoomSummary): void;
  /** Open an honest slice-status notice for management. */
  onManage(room: RoomSummary): void;
}

/** Render a room row with separate open and management controls. */
export function RoomRow({ room, onOpen, onManage }: RoomRowProps) {
  return (
    <article className="room-row">
      <button className="room-row-main" type="button" onClick={() => onOpen(room)}>
        <span className="room-glyph" data-status={room.status} aria-hidden="true">
          #
        </span>
        <span className="room-copy">
          <span className="room-title-line">
            <strong>{room.name}</strong>
            <span>{room.serverName}</span>
          </span>
          <span className="room-preview-line">
            {room.latestAuthor ? <b>{room.latestAuthor}</b> : null}
            {room.preview}
          </span>
        </span>
      </button>

      <div className="room-row-context">
        <div className="row-signals" aria-label={roomStatusLabel(room.status)}>
          {room.activeWork ? <ListTodo aria-label="Active work" /> : null}
          {room.pendingApprovals > 0 ? (
            <BellDot aria-label={`${room.pendingApprovals} pending approval`} />
          ) : null}
          {room.status === "paused" ? <CirclePause aria-label="Bridge paused" /> : null}
        </div>
        <ParticipantStack participants={room.participants} limit={3} />
        {room.unreadCount > 0 ? (
          <span className="row-unread" aria-label={`${room.unreadCount} unread messages`}>
            {room.unreadCount}
          </span>
        ) : null}
        <time dateTime={room.lastActivityAt}>{formatRelativeTime(room.lastActivityAt)}</time>
        <button
          className="icon-button"
          type="button"
          aria-label={`Manage ${room.name}`}
          onClick={() => onManage(room)}
        >
          <MoreHorizontal aria-hidden="true" />
        </button>
      </div>
    </article>
  );
}
