/** Deterministic lightweight spatial layout for sanitized Henosis rooms. */
import type { RoomSummary } from "./rooms";

/** Stable three-dimensional position assigned to one room. */
export interface SpatialRoomPoint {
  /** Room represented by this point. */
  room: RoomSummary;
  /** Horizontal world coordinate. */
  x: number;
  /** Vertical world coordinate. */
  y: number;
  /** Depth world coordinate. */
  z: number;
}

/** Two-dimensional projection used by the accessible DOM renderer. */
export interface ProjectedRoomPoint extends SpatialRoomPoint {
  /** Percentage from the left edge of the field. */
  screenX: number;
  /** Percentage from the top edge of the field. */
  screenY: number;
  /** Normalized depth used for scale, opacity, and ordering. */
  depth: number;
}

/** Hash one stable room identifier without depending on response order. */
function hashRoomId(roomId: string): number {
  let hash = 2166136261;
  for (const character of roomId) {
    hash ^= character.codePointAt(0) ?? 0;
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

/** Place rooms across bounded rings with deterministic height variation. */
export function buildSpatialRoomPoints(
  rooms: readonly RoomSummary[],
): SpatialRoomPoint[] {
  return [...rooms]
    .sort((left, right) => left.id.localeCompare(right.id))
    .map((room, index, orderedRooms) => {
      const hash = hashRoomId(room.id);
      const angle =
        (index / Math.max(orderedRooms.length, 1)) * Math.PI * 2 +
        ((hash % 29) / 29) * 0.34;
      const radius = 0.54 + ((hash >>> 5) % 29) / 100;
      return {
        room,
        x: Math.cos(angle) * radius,
        y: (((hash >>> 11) % 31) - 15) / 70,
        z: Math.sin(angle) * radius,
      };
    });
}

/** Project world points through bounded yaw and pitch for a DOM spatial field. */
export function projectSpatialRoomPoints(
  points: readonly SpatialRoomPoint[],
  yaw: number,
  pitch: number,
): ProjectedRoomPoint[] {
  const yawRadians = (yaw * Math.PI) / 180;
  const pitchRadians = (pitch * Math.PI) / 180;
  const yawCosine = Math.cos(yawRadians);
  const yawSine = Math.sin(yawRadians);
  const pitchCosine = Math.cos(pitchRadians);
  const pitchSine = Math.sin(pitchRadians);

  return points
    .map((point) => {
      const rotatedX = point.x * yawCosine - point.z * yawSine;
      const yawDepth = point.x * yawSine + point.z * yawCosine;
      const rotatedY = point.y * pitchCosine - yawDepth * pitchSine;
      const depth = point.y * pitchSine + yawDepth * pitchCosine;
      const perspective = 0.82 + (depth + 0.8) * 0.18;
      return {
        ...point,
        screenX: 50 + rotatedX * 45 * perspective,
        screenY: 48 + rotatedY * 55 * perspective,
        depth,
      };
    })
    .sort((left, right) => left.depth - right.depth);
}
