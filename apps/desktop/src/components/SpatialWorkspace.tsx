/** Spatial renderer that turns current Henosis rooms and participants into places. */
import { useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import type { KeyboardEvent, PointerEvent } from "react";
import { ArrowRight, BellDot, Crosshair, RotateCcw, Users } from "lucide-react";
import type { RoomSummary } from "../domain/rooms";
import {
  buildSpatialRoomPoints,
  projectSpatialRoomPoints,
} from "../domain/spatialLayout";
import type { RoomDirectorySnapshot } from "../services/henosisClient";

/** Inputs required by the spatial room renderer. */
export interface SpatialWorkspaceProps {
  /** Current sanitized room directory. */
  directory: RoomDirectorySnapshot;
  /** Enter one room through the shared application state. */
  onOpenRoom(room: RoomSummary): void;
  /** Graphical profile selector retained inside the spatial chrome. */
  experienceSelector: ReactNode;
}

/** Active pointer drag retained outside React render state. */
interface SpatialDrag {
  /** Pointer identifier owning the current drag. */
  pointerId: number;
  /** Previous horizontal pointer position. */
  x: number;
  /** Previous vertical pointer position. */
  y: number;
}

/** Keep pitch inside a readable range that never flips the room plane. */
function clampPitch(pitch: number): number {
  return Math.max(-24, Math.min(24, pitch));
}

/** Render a deterministic, keyboard-operable field with a visible list fallback. */
export function SpatialWorkspace({
  directory,
  onOpenRoom,
  experienceSelector,
}: SpatialWorkspaceProps) {
  const [yaw, setYaw] = useState(12);
  const [pitch, setPitch] = useState(-7);
  const dragRef = useRef<SpatialDrag | null>(null);
  const points = useMemo(
    () => buildSpatialRoomPoints(directory.rooms),
    [directory.rooms],
  );
  const projected = useMemo(
    () => projectSpatialRoomPoints(points, yaw, pitch),
    [pitch, points, yaw],
  );
  const agentCount = directory.rooms.reduce(
    (count, room) =>
      count + room.participants.filter((participant) => participant.isAgent).length,
    0,
  );

  /** Rotate the field in bounded steps while the stage has keyboard focus. */
  function handleKeyDown(event: KeyboardEvent<HTMLDivElement>): void {
    const step = event.shiftKey ? 12 : 5;
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      setYaw((current) => current - step);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      setYaw((current) => current + step);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      setPitch((current) => clampPitch(current - step));
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      setPitch((current) => clampPitch(current + step));
    } else if (event.key === "Home") {
      event.preventDefault();
      resetView();
    }
  }

  /** Begin one pointer-owned orbit gesture. */
  function handlePointerDown(event: PointerEvent<HTMLDivElement>): void {
    if (event.target instanceof Element && event.target.closest("button")) {
      return;
    }
    dragRef.current = {
      pointerId: event.pointerId,
      x: event.clientX,
      y: event.clientY,
    };
    event.currentTarget.setPointerCapture(event.pointerId);
  }

  /** Project pointer movement into yaw and bounded pitch. */
  function handlePointerMove(event: PointerEvent<HTMLDivElement>): void {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) {
      return;
    }
    setYaw((current) => current + (event.clientX - drag.x) * 0.22);
    setPitch((current) =>
      clampPitch(current - (event.clientY - drag.y) * 0.16),
    );
    drag.x = event.clientX;
    drag.y = event.clientY;
  }

  /** Release the current orbit gesture without changing the selected room. */
  function handlePointerUp(event: PointerEvent<HTMLDivElement>): void {
    if (dragRef.current?.pointerId === event.pointerId) {
      dragRef.current = null;
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  }

  /** Restore the authored overview orientation. */
  function resetView(): void {
    setYaw(12);
    setPitch(-7);
  }

  return (
    <main className="spatial-workspace" id="main-content">
      <header className="spatial-header">
        <div>
          <p className="eyebrow">Spatial environment</p>
          <h1>Rooms are places. Agents have a seat.</h1>
          <p>
            Orbit the live room field, then enter the place where work is
            moving.
          </p>
        </div>
        <dl>
          <div>
            <dt>Rooms</dt>
            <dd>{directory.rooms.length}</dd>
          </div>
          <div>
            <dt>Agents present</dt>
            <dd>{agentCount}</dd>
          </div>
          <div>
            <dt>Approvals</dt>
            <dd>
              {directory.rooms.reduce(
                (count, room) => count + room.pendingApprovals,
                0,
              )}
            </dd>
          </div>
        </dl>
        {experienceSelector}
      </header>

      <div className="spatial-layout">
        <section className="spatial-map" aria-labelledby="spatial-map-title">
          <div className="spatial-map-toolbar">
            <div>
              <Crosshair aria-hidden="true" />
              <span>
                <strong id="spatial-map-title">Live room field</strong>
                <small>Drag to orbit. Arrow keys rotate. Home resets.</small>
              </span>
            </div>
            <button type="button" onClick={resetView}>
              <RotateCcw aria-hidden="true" />
              Reset view
            </button>
          </div>

          <div
            className="spatial-field"
            role="group"
            tabIndex={0}
            aria-label="Interactive spatial room field"
            onKeyDown={handleKeyDown}
            onPointerDown={handlePointerDown}
            onPointerMove={handlePointerMove}
            onPointerUp={handlePointerUp}
            onPointerCancel={handlePointerUp}
          >
            <span className="spatial-orbit spatial-orbit--outer" aria-hidden="true" />
            <span className="spatial-orbit spatial-orbit--inner" aria-hidden="true" />
            <span className="spatial-origin" aria-hidden="true">
              <i />
              Henosis
            </span>
            {projected.map((point, index) => {
              const agents = point.room.participants.filter(
                (participant) => participant.isAgent,
              );
              const scale = Math.max(0.76, Math.min(1.12, 0.94 + point.depth * 0.18));
              return (
                <button
                  className="spatial-room"
                  type="button"
                  data-status={point.room.status}
                  aria-label={`Enter room ${point.room.name}`}
                  style={{
                    left: `${point.screenX}%`,
                    top: `${point.screenY}%`,
                    zIndex: index + 2,
                    transform: `translate(-50%, -50%) scale(${scale})`,
                  }}
                  onClick={() => onOpenRoom(point.room)}
                  key={point.room.id}
                >
                  <span className="spatial-room-beacon" aria-hidden="true" />
                  <span className="spatial-room-copy">
                    <small>{point.room.serverName ?? "Rift"}</small>
                    <strong>#{point.room.name}</strong>
                    <em>{point.room.activeWork ?? point.room.preview}</em>
                  </span>
                  <span className="spatial-room-meta">
                    <span>
                      <Users aria-hidden="true" />
                      {point.room.participants.length}
                    </span>
                    {point.room.pendingApprovals > 0 ? (
                      <span data-attention="true">
                        <BellDot aria-hidden="true" />
                        {point.room.pendingApprovals}
                      </span>
                    ) : null}
                  </span>
                  {agents.length > 0 ? (
                    <span className="spatial-agent-seats" aria-label="Agents present">
                      {agents.slice(0, 3).map((agent) => (
                        <i title={agent.displayName} key={agent.id}>
                          {agent.displayName.slice(0, 1).toLocaleUpperCase()}
                        </i>
                      ))}
                    </span>
                  ) : null}
                </button>
              );
            })}
          </div>
        </section>

        <aside className="spatial-room-index" aria-label="Spatial room index">
          <p className="eyebrow">Direct route</p>
          <h2>Every room in reach</h2>
          <p>
            The index mirrors the field for touch, keyboard, and reduced-motion
            navigation.
          </p>
          <ol>
            {directory.rooms.map((room) => (
              <li key={room.id}>
                <button type="button" onClick={() => onOpenRoom(room)}>
                  <span data-status={room.status} aria-hidden="true" />
                  <span>
                    <strong>#{room.name}</strong>
                    <small>{room.activeWork ?? room.serverName}</small>
                  </span>
                  <ArrowRight aria-hidden="true" />
                </button>
              </li>
            ))}
          </ol>
        </aside>
      </div>
    </main>
  );
}
