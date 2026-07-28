/** Large automatic newest-room card that anchors the Henosis home. */
import { ArrowUpRight, CheckCircle2, MessageCircle, Sparkles } from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import {
  formatRelativeTime,
  ParticipantStack,
  roomStatusLabel,
} from "./roomPresentation";

/** Inputs for the current automatic pinned room. */
export interface PinnedRoomProps {
  /** Newest room after search and filter rules. */
  room: RoomSummary;
  /** Enter the room through a visible GUI action. */
  onOpen(room: RoomSummary): void;
}

/** Render the newest room as the selector's primary continuation action. */
export function PinnedRoom({ room, onOpen }: PinnedRoomProps) {
  return (
    <article className="pinned-room reveal">
      <div className="confluence-lines" aria-hidden="true">
        <span />
        <span />
        <span />
        <span />
      </div>

      <div className="pinned-topline">
        <div className="pin-label">
          <Sparkles aria-hidden="true" />
          Most recent room
        </div>
        <span className="activity-time">{formatRelativeTime(room.lastActivityAt)}</span>
      </div>

      <div className="pinned-content">
        <div>
          <p className="room-parent">{room.serverName ?? "Rift room"}</p>
          <h2>#{room.name}</h2>
          <p className="pinned-preview">
            {room.latestAuthor ? <strong>{room.latestAuthor}: </strong> : null}
            {room.preview}
          </p>
        </div>

        <div className="pinned-state">
          <span className="status-pill" data-status={room.status}>
            <span className="status-dot" aria-hidden="true" />
            {roomStatusLabel(room.status)}
          </span>
          {room.unreadCount > 0 ? (
            <span className="unread-pill">
              <MessageCircle aria-hidden="true" />
              {room.unreadCount} unread
            </span>
          ) : (
            <span className="read-pill">
              <CheckCircle2 aria-hidden="true" />
              Caught up
            </span>
          )}
        </div>
      </div>

      <div className="pinned-footer">
        <div className="pinned-context">
          <ParticipantStack participants={room.participants} />
          <span className="context-divider" aria-hidden="true" />
          <p>
            <span>Current thread</span>
            <strong>{room.activeWork ?? room.topic ?? "Open conversation"}</strong>
          </p>
        </div>
        <button
          className="button button-primary continue-button"
          type="button"
          onClick={() => onOpen(room)}
        >
          Continue room
          <ArrowUpRight aria-hidden="true" />
        </button>
      </div>
    </article>
  );
}
