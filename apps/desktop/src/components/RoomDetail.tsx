/** Integrated room conversation workspace reached from the room selector. */
import { ArrowLeft, BellDot, PanelRight } from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import type { HenosisClient } from "../services/henosisClient";
import { ParticipantStack, roomStatusLabel } from "./roomPresentation";
import { RoomConversation } from "./RoomConversation";

/** Inputs for entering one room from the room selector. */
export interface RoomDetailProps {
  /** Shared native or fixture adapter owned by the application shell. */
  client: HenosisClient;
  /** Selected room summary. */
  room: RoomSummary;
  /** Return to the room selector without leaving Henosis. */
  onBack(): void;
  /** Explain approval and dashboard controls scheduled for later workspace slices. */
  onDeferredAction(action: string): void;
}

/** Render a visible room workspace instead of hiding Rift behind a terminal. */
export function RoomDetail({
  client,
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
              className="button button-secondary room-approvals-button"
              type="button"
              onClick={() => onDeferredAction("Open room approvals")}
            >
              <BellDot aria-hidden="true" />
              {room.pendingApprovals} waiting
            </button>
          ) : null}
          <button
            className="room-dashboard-trigger"
            type="button"
            aria-label="Open room dashboard"
            onClick={() => onDeferredAction("Open room dashboard")}
          >
            <PanelRight aria-hidden="true" />
            <span>Dashboard</span>
          </button>
        </div>
      </header>

      <div className="room-detail-grid">
        <RoomConversation client={client} roomId={room.id} />

        <aside className="room-dashboard" aria-label="Room dashboard">
          <p className="eyebrow">Room dashboard</p>
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
