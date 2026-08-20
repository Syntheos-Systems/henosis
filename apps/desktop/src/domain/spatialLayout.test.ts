/** Unit tests for deterministic Henosis spatial projection. */
import { describe, expect, it } from "vitest";
import { createFixtureRooms } from "../data/fixtureRooms";
import {
  buildSpatialRoomPoints,
  projectSpatialRoomPoints,
} from "./spatialLayout";

describe("spatial layout", () => {
  it("keeps room positions stable when the directory response is reordered", () => {
    const rooms = createFixtureRooms(new Date("2026-07-26T18:00:00.000Z"));
    const forward = buildSpatialRoomPoints(rooms);
    const reversed = buildSpatialRoomPoints([...rooms].reverse());
    expect(reversed).toEqual(forward);
  });

  it("projects every room into a bounded readable field", () => {
    const rooms = createFixtureRooms(new Date("2026-07-26T18:00:00.000Z"));
    const projected = projectSpatialRoomPoints(
      buildSpatialRoomPoints(rooms),
      18,
      -8,
    );
    expect(projected).toHaveLength(rooms.length);
    for (const point of projected) {
      expect(point.screenX).toBeGreaterThanOrEqual(8);
      expect(point.screenX).toBeLessThanOrEqual(92);
      expect(point.screenY).toBeGreaterThanOrEqual(8);
      expect(point.screenY).toBeLessThanOrEqual(88);
    }
  });
});
