/** First visible room threshold reached from the selector in slice 1. */
import {
  ArrowLeft,
  BellDot,
  MessageSquareText,
  PanelRight,
  Send,
} from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import {
  formatRelativeTime,
  ParticipantStack,
  roomStatusLabel,
} from "./roomPresentation";

/** Inputs for entering one room from the room selector. */
export interface RoomDetailProps {
  /** Selected room summary. */
  room: RoomSummary;
  /** Return to the room selector without leaving Henosis. */
  onBack(): void;
  /** Explain chat controls scheduled for the full Rift slice. */
  onDeferredAction(action: string): void;
}

/** Render a visible room workspace instead of hiding Rift behind a terminal. */
export function RoomDetail({
  room,
  onBack,
  onDeferredAction,
}: RoomDetailProps) {
  return (
    <main className="room-detail" id="main-content">
      <header className="room-detail-header">
        <button className="back-button" type="button" onClick={onBack}>
          <ArrowLeft aria-hidden="true" />
          All rooms
        </button>
        <div className="room-detail-title">
          <span className="room-detail-glyph" aria-hidden="true">
            #
          </span>
          <div>
            <p>{room.serverName}</p>
            <h1>{room.name}</h1>
          </div>
        </div>
        <div className="room-detail-actions">
          {room.pendingApprovals > 0 ? (
            <button
              className="button button-secondary"
              type="button"
              onClick={() => onDeferredAction("Open room approvals")}
            >
              <BellDot aria-hidden="true" />
              {room.pendingApprovals} waiting
            </button>
          ) : null}
          <button
            className="icon-button"
            type="button"
            aria-label="Open room context"
            onClick={() => onDeferredAction("Open room context")}
          >
            <PanelRight aria-hidden="true" />
          </button>
        </div>
      </header>

      <div className="room-detail-grid">
        <section className="room-conversation" aria-labelledby="conversation-title">
          <div className="conversation-date">
            <span />
            <p id="conversation-title">Latest room activity</p>
            <span />
          </div>

          <article className="message-card">
            <span className="message-avatar" aria-hidden="true">
              {(room.latestAuthor ?? "H").slice(0, 2).toLocaleUpperCase()}
            </span>
            <div>
              <header>
                <strong>{room.latestAuthor ?? "Henosis"}</strong>
                <time dateTime={room.lastActivityAt}>
                  {formatRelativeTime(room.lastActivityAt)} ago
                </time>
              </header>
              <p>{room.preview}</p>
            </div>
          </article>

          <div className="conversation-boundary">
            <MessageSquareText aria-hidden="true" />
            <div>
              <strong>The room is visible. Full conversation sync is next.</strong>
              <p>
                Slice 2 connects history, live agent replies, editing, uploads,
                presence, and direct messages here. Henosis will not route
                those operations through a terminal.
              </p>
            </div>
          </div>

          <div className="composer-preview">
            <label htmlFor="room-composer">Message #{room.name}</label>
            <div>
              <textarea
                id="room-composer"
                rows={2}
                placeholder="The live composer arrives with full Rift sync in slice 2."
                disabled
              />
              <button
                className="icon-button send-button"
                type="button"
                aria-label="Send message"
                onClick={() => onDeferredAction("Send a room message")}
              >
                <Send aria-hidden="true" />
              </button>
            </div>
          </div>
        </section>

        <aside className="room-context" aria-label="Room context">
          <p className="eyebrow">Room context</p>
          <h2>{room.topic ?? "Persistent conversation"}</h2>

          <dl>
            <div>
              <dt>State</dt>
              <dd>
                <span className="status-dot" data-status={room.status} />
                {roomStatusLabel(room.status)}
              </dd>
            </div>
            <div>
              <dt>Current thread</dt>
              <dd>{room.activeWork ?? "Open conversation"}</dd>
            </div>
            <div>
              <dt>Unread</dt>
              <dd>{room.unreadCount} messages</dd>
            </div>
          </dl>

          <div className="context-participants">
            <span>People and agents</span>
            <ParticipantStack participants={room.participants} limit={6} />
          </div>
        </aside>
      </div>
    </main>
  );
}
