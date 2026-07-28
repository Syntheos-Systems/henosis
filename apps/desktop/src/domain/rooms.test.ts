/** Behavioral tests for automatic pinning, filtering, and recency grouping. */
import { describe, expect, it } from "vitest";
import { createFixtureRooms } from "../data/fixtureRooms";
import {
  filterRooms,
  groupRoomsByRecency,
  pinNewestRoom,
} from "./rooms";
/** Room contract used to build deterministic test data. */
import type { RoomSummary } from "./rooms";

/** Stable clock shared by room-domain tests. */
const NOW = new Date("2026-07-26T18:00:00.000Z");

describe("pinNewestRoom", () => {
  it("pins exactly the newest room and sorts the remainder newest-first", () => {
    const rooms = createFixtureRooms(NOW);
    const originalOrder = rooms.map((room) => room.id);

    const result = pinNewestRoom([...rooms].reverse());

    expect(result.pinned?.id).toBe("room-orchard");
    expect(result.remaining.map((room) => room.id)).toEqual([
      "room-rift",
      "room-governance",
      "room-nightwatch",
      "room-archive",
    ]);
    expect(rooms.map((room) => room.id)).toEqual(originalOrder);
  });

  it("uses room name and id as stable tie breakers", () => {
    const tied: RoomSummary[] = [
      {
        ...createFixtureRooms(NOW)[0],
        id: "room-z",
        name: "Zulu",
      },
      {
        ...createFixtureRooms(NOW)[0],
        id: "room-b",
        name: "alpha",
      },
      {
        ...createFixtureRooms(NOW)[0],
        id: "room-a",
        name: "Alpha",
      },
    ];

    const result = pinNewestRoom(tied);

    expect([result.pinned, ...result.remaining].map((room) => room?.id)).toEqual([
      "room-a",
      "room-b",
      "room-z",
    ]);
  });

  it("returns an empty pin for an empty directory", () => {
    expect(pinNewestRoom([])).toEqual({ pinned: null, remaining: [] });
  });
});

describe("filterRooms", () => {
  it("searches room, server, author, preview, and participant names", () => {
    const rooms = createFixtureRooms(NOW);

    expect(filterRooms(rooms, "trust lab", new Set()).map((room) => room.id)).toEqual([
      "room-governance",
    ]);
    expect(filterRooms(rooms, "cinder", new Set()).map((room) => room.id)).toEqual([
      "room-orchard",
      "room-rift",
    ]);
    expect(filterRooms(rooms, "cursor", new Set()).map((room) => room.id)).toEqual([
      "room-rift",
    ]);
  });

  it("composes selected status filters without mutating rooms", () => {
    const rooms = createFixtureRooms(NOW);

    const filtered = filterRooms(
      rooms,
      "",
      new Set(["unread", "active-work", "approvals"]),
    );

    expect(filtered.map((room) => room.id)).toEqual(["room-governance"]);
    expect(rooms).toHaveLength(5);
  });
});

describe("groupRoomsByRecency", () => {
  it("groups rooms into Today, Previous 7 days, and Older", () => {
    const rooms = createFixtureRooms(NOW);
    const grouped = groupRoomsByRecency(rooms, NOW);

    expect(grouped.map((group) => group.label)).toEqual([
      "Today",
      "Previous 7 days",
      "Older",
    ]);
    expect(grouped[0].rooms.map((room) => room.id)).toEqual([
      "room-orchard",
      "room-rift",
      "room-governance",
    ]);
    expect(grouped[1].rooms.map((room) => room.id)).toEqual(["room-nightwatch"]);
    expect(grouped[2].rooms.map((room) => room.id)).toEqual(["room-archive"]);
  });
});
