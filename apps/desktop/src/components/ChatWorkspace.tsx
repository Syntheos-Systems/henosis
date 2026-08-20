/** Conversation-first renderer for people who prefer a familiar chat layout. */
import type { ReactNode } from "react";
import { BellDot, Hash, LayoutDashboard, Users } from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import type {
  HenosisClient,
  RoomDirectorySnapshot,
} from "../services/henosisClient";
import { RoomConversation } from "./RoomConversation";

/** Inputs required by the chat-focused room renderer. */
export interface ChatWorkspaceProps {
  /** Shared native or fixture adapter. */
  client: HenosisClient;
  /** Current sanitized room directory. */
  directory: RoomDirectorySnapshot;
  /** Room currently open in the shared application state. */
  selectedRoom?: RoomSummary;
  /** Open one room without changing renderer profile. */
  onOpenRoom(room: RoomSummary): void;
  /** Reveal the complete room controls while preserving the room. */
  onOpenDesktop(): void;
  /** Graphical profile selector retained inside the chat chrome. */
  experienceSelector: ReactNode;
}

/** Render rooms beside a single primary conversation surface. */
export function ChatWorkspace({
  client,
  directory,
  selectedRoom,
  onOpenRoom,
  onOpenDesktop,
  experienceSelector,
}: ChatWorkspaceProps) {
  return (
    <main className="chat-workspace" id="main-content">
      <div className="chat-profile-bar">{experienceSelector}</div>
      <aside className="chat-room-list" aria-label="Rooms">
        <header>
          <p className="eyebrow">Conversation mode</p>
          <h1>{directory.connection?.displayName ?? "Henosis"}</h1>
          <span>
            {directory.connected ? "Rift connected" : "Cached room history"}
          </span>
        </header>
        <nav aria-label="Chat rooms">
          {directory.rooms.map((room) => (
            <button
              className="chat-room-link"
              type="button"
              data-active={selectedRoom?.id === room.id}
              aria-current={selectedRoom?.id === room.id ? "page" : undefined}
              onClick={() => onOpenRoom(room)}
              key={room.id}
            >
              <Hash aria-hidden="true" />
              <span>
                <strong>{room.name}</strong>
                <small>{room.preview}</small>
              </span>
              {room.pendingApprovals > 0 ? (
                <b aria-label={`${room.pendingApprovals} approvals waiting`}>
                  {room.pendingApprovals}
                </b>
              ) : room.unreadCount > 0 ? (
                <b aria-label={`${room.unreadCount} unread messages`}>
                  {room.unreadCount}
                </b>
              ) : null}
            </button>
          ))}
        </nav>
      </aside>

      <section className="chat-conversation" aria-label="Chat workspace">
        {selectedRoom ? (
          <>
            <header className="chat-conversation-header">
              <div>
                <Hash aria-hidden="true" />
                <span>
                  <strong>{selectedRoom.name}</strong>
                  <small>{selectedRoom.topic ?? selectedRoom.serverName}</small>
                </span>
              </div>
              <div className="chat-room-signals">
                {selectedRoom.pendingApprovals > 0 ? (
                  <span data-attention="true">
                    <BellDot aria-hidden="true" />
                    {selectedRoom.pendingApprovals} waiting
                  </span>
                ) : null}
                <span>
                  <Users aria-hidden="true" />
                  {selectedRoom.participants.length}
                </span>
                <button type="button" onClick={onOpenDesktop}>
                  <LayoutDashboard aria-hidden="true" />
                  Room controls
                </button>
              </div>
            </header>
            <RoomConversation client={client} roomId={selectedRoom.id} />
          </>
        ) : (
          <div className="chat-empty">
            <MessageSquareGlyph />
            <p className="eyebrow">Choose a room</p>
            <h2>Conversation without the control room.</h2>
            <p>
              Select a room to keep messages, people, and current work in one
              focused view.
            </p>
          </div>
        )}
      </section>
    </main>
  );
}

/** Render a small product-native empty-state glyph without an image asset. */
function MessageSquareGlyph() {
  return (
    <span className="chat-empty-glyph" aria-hidden="true">
      <i />
      <i />
      <i />
    </span>
  );
}
