/** Codex-style Henosis home that selects and enters Rift rooms. */
import { useMemo, useState } from "react";
import { DoorOpen, Plus, RotateCw } from "lucide-react";
import {
  filterRooms,
  groupRoomsByRecency,
  pinNewestRoom,
} from "../domain/rooms";
/** Room contracts consumed by the selector without adding runtime imports. */
import type { RoomFilter, RoomSummary } from "../domain/rooms";
import type { RoomDirectorySnapshot } from "../services/henosisClient";
import { PinnedRoom } from "./PinnedRoom";
import { RoomFilters } from "./RoomFilters";
import { RoomRow } from "./RoomRow";
import { StatusNotice } from "./StatusNotice";

/** Inputs and actions exposed by the complete room-selector view. */
export interface RoomDirectoryProps {
  /** Live, cached, or fixture-backed room snapshot. */
  directory: RoomDirectorySnapshot;
  /** True while a native refresh is running. */
  refreshing: boolean;
  /** Enter one selected room. */
  onOpenRoom(room: RoomSummary): void;
  /** Refresh room activity through the active adapter. */
  onRefresh(): void;
  /** Open first-run setup to repair a connection. */
  onReconnect(): void;
  /** Explain an operation scheduled for a later delivery slice. */
  onDeferredAction(action: string): void;
}

/** Render the room selector with an automatic newest-room pin. */
export function RoomDirectory({
  directory,
  refreshing,
  onOpenRoom,
  onRefresh,
  onReconnect,
  onDeferredAction,
}: RoomDirectoryProps) {
  const [query, setQuery] = useState("");
  const [filters, setFilters] = useState<Set<RoomFilter>>(new Set());

  /** Toggle one filter while preserving the other selected filters. */
  function handleFilterToggle(filter: RoomFilter) {
    setFilters((current) => {
      const next = new Set(current);
      if (next.has(filter)) {
        next.delete(filter);
      } else {
        next.add(filter);
      }
      return next;
    });
  }

  const filteredRooms = useMemo(
    () => filterRooms(directory.rooms, query, filters),
    [directory.rooms, filters, query],
  );
  const { pinned, remaining } = useMemo(
    () => pinNewestRoom(filteredRooms),
    [filteredRooms],
  );
  const groups = useMemo(() => groupRoomsByRecency(remaining), [remaining]);
  const filterActive = query.trim().length > 0 || filters.size > 0;

  return (
    <main className="room-directory" id="main-content">
      <header className="directory-header reveal">
        <div>
          <p className="eyebrow">Rift rooms</p>
          <h1>Return to the current.</h1>
          <p className="directory-lede">
            Step back into a persistent room. People can leave; the work keeps
            moving.
          </p>
        </div>
        <div className="directory-actions">
          <button
            className="button button-secondary"
            type="button"
            onClick={() => onDeferredAction("Join room")}
          >
            <DoorOpen aria-hidden="true" />
            Join room
          </button>
          <button
            className="button button-primary"
            type="button"
            onClick={() => onDeferredAction("New room")}
          >
            <Plus aria-hidden="true" />
            New room
          </button>
        </div>
      </header>

      <StatusNotice
        source={directory.source}
        connected={directory.connected}
        onReconnect={onReconnect}
      />

      <RoomFilters
        query={query}
        filters={filters}
        onQueryChange={setQuery}
        onFilterToggle={handleFilterToggle}
      />

      {pinned ? (
        <section aria-labelledby="pinned-room-heading">
          <h2 className="sr-only" id="pinned-room-heading">
            Most recent room
          </h2>
          <PinnedRoom room={pinned} onOpen={onOpenRoom} />
        </section>
      ) : (
        <section className="room-empty" aria-labelledby="empty-title">
          <span aria-hidden="true">#</span>
          <h2 id="empty-title">
            {filterActive ? "No rooms match this view." : "No rooms yet."}
          </h2>
          <p>
            {filterActive
              ? "Clear a filter or search for a different person, server, or message."
              : "Create a room or join one with an invitation. No terminal required."}
          </p>
          {filterActive ? (
            <button
              className="button button-secondary"
              type="button"
              onClick={() => {
                setQuery("");
                setFilters(new Set());
              }}
            >
              Clear filters
            </button>
          ) : null}
        </section>
      )}

      {groups.length > 0 ? (
        <section className="room-history" aria-labelledby="room-history-title">
          <div className="history-heading">
            <div>
              <p className="eyebrow">Room history</p>
              <h2 id="room-history-title">Everything still within reach</h2>
            </div>
            <button
              className="icon-button refresh-button"
              type="button"
              onClick={onRefresh}
              aria-label="Refresh room activity"
              disabled={refreshing}
            >
              <RotateCw data-spinning={refreshing} aria-hidden="true" />
            </button>
          </div>

          {groups.map((group) => (
            <div className="room-group" key={group.label}>
              <h3>
                {group.label}
                <span>{group.rooms.length}</span>
              </h3>
              <div className="room-list">
                {group.rooms.map((room) => (
                  <RoomRow
                    room={room}
                    onOpen={onOpenRoom}
                    onManage={() => onDeferredAction(`Manage #${room.name}`)}
                    key={room.id}
                  />
                ))}
              </div>
            </div>
          ))}
        </section>
      ) : null}
    </main>
  );
}
