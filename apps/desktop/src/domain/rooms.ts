/**
 * Pure room-directory rules shared by the Henosis selector and its tests.
 *
 * A room is the user-facing Rift channel. The selector never asks a person to
 * understand the server/channel storage hierarchy before entering one.
 */

/** Visual and operational states shown on room summaries. */
export type RoomStatus =
  | "quiet"
  | "active"
  | "paused"
  | "disconnected"
  | "awaiting-approval";

/** Data provenance shown by the selector so stale data never masquerades as live. */
export type DirectorySource = "live" | "cached" | "fixture";

/** A human or agent represented in the room selector. */
export interface RoomParticipant {
  /** Stable Rift user identifier. */
  id: string;
  /** Human-readable name used in avatars and search. */
  displayName: string;
  /** Optional image URL supplied by Rift. */
  avatarUrl?: string;
  /** Distinguishes persistent agents from humans. */
  isAgent: boolean;
  /** Current Rift presence state when known. */
  presence?: string;
}

/** Sanitized room data safe to expose to the webview. */
export interface RoomSummary {
  /** Opaque Rift channel identifier. */
  id: string;
  /** Channel name presented as the room name. */
  name: string;
  /** Parent Rift server identifier. */
  serverId: string;
  /** Parent Rift server name. */
  serverName?: string;
  /** Optional room topic. */
  topic?: string;
  /** Latest message or a useful empty-room description. */
  preview: string;
  /** Display name of the latest message author. */
  latestAuthor?: string;
  /** ISO timestamp for latest activity or room creation. */
  lastActivityAt: string;
  /** Present or recently relevant humans and agents. */
  participants: RoomParticipant[];
  /** Number of unseen messages when known. */
  unreadCount: number;
  /** Current room state derived from Rift and Henosis bridge data. */
  status: RoomStatus;
  /** Optional active task or cascade summary. */
  activeWork?: string;
  /** Number of approvals currently waiting on a person. */
  pendingApprovals: number;
}

/** Toggleable room-directory filters. */
export type RoomFilter = "unread" | "active-work" | "paused" | "approvals";

/** Result of extracting the automatic pinned room. */
export interface PinnedRooms {
  /** The single room with the newest activity. */
  pinned: RoomSummary | null;
  /** Remaining rooms in descending activity order. */
  remaining: RoomSummary[];
}

/** Product-defined recency buckets used by the room history. */
export type RoomGroupLabel = "Today" | "Previous 7 days" | "Older";

/** One labeled recency bucket and its ordered rooms. */
export interface RoomGroup {
  /** Human-readable bucket label. */
  label: RoomGroupLabel;
  /** Rooms in descending activity order. */
  rooms: RoomSummary[];
}

/** Parse a room activity timestamp into a stable sortable epoch. */
function activityEpoch(room: RoomSummary): number {
  const parsed = Date.parse(room.lastActivityAt);
  return Number.isNaN(parsed) ? 0 : parsed;
}

/** Compare rooms newest-first with deterministic tie breakers. */
export function compareRooms(left: RoomSummary, right: RoomSummary): number {
  const activityDifference = activityEpoch(right) - activityEpoch(left);
  if (activityDifference !== 0) {
    return activityDifference;
  }

  const nameDifference = left.name.localeCompare(right.name, undefined, {
    sensitivity: "base",
  });
  return nameDifference !== 0 ? nameDifference : left.id.localeCompare(right.id);
}

/** Select exactly one automatic pinned room without mutating the input. */
export function pinNewestRoom(rooms: readonly RoomSummary[]): PinnedRooms {
  const sorted = [...rooms].sort(compareRooms);
  return {
    pinned: sorted[0] ?? null,
    remaining: sorted.slice(1),
  };
}

/** Test whether a room satisfies one selected operational filter. */
function matchesFilter(room: RoomSummary, filter: RoomFilter): boolean {
  switch (filter) {
    case "unread":
      return room.unreadCount > 0;
    case "active-work":
      return Boolean(room.activeWork) || room.status === "active";
    case "paused":
      return room.status === "paused";
    case "approvals":
      return room.pendingApprovals > 0 || room.status === "awaiting-approval";
  }
}

/** Apply text and operational filters without reordering or mutating rooms. */
export function filterRooms(
  rooms: readonly RoomSummary[],
  query: string,
  filters: ReadonlySet<RoomFilter>,
): RoomSummary[] {
  const normalizedQuery = query.trim().toLocaleLowerCase();

  return rooms.filter((room) => {
    const searchCorpus = [
      room.name,
      room.serverName,
      room.preview,
      room.latestAuthor,
      ...room.participants.map((participant) => participant.displayName),
    ]
      .filter(Boolean)
      .join(" ")
      .toLocaleLowerCase();

    const matchesQuery =
      normalizedQuery.length === 0 || searchCorpus.includes(normalizedQuery);
    const matchesSelectedFilters = [...filters].every((filter) =>
      matchesFilter(room, filter),
    );
    return matchesQuery && matchesSelectedFilters;
  });
}

/** Calculate the local start of the current calendar day. */
function startOfLocalDay(value: Date): Date {
  const start = new Date(value);
  start.setHours(0, 0, 0, 0);
  return start;
}

/** Group ordered rooms into the selector's three recency sections. */
export function groupRoomsByRecency(
  rooms: readonly RoomSummary[],
  now: Date = new Date(),
): RoomGroup[] {
  const startOfToday = startOfLocalDay(now).getTime();
  const sevenDaysAgo = startOfToday - 6 * 24 * 60 * 60 * 1000;
  const groups = new Map<RoomGroupLabel, RoomSummary[]>([
    ["Today", []],
    ["Previous 7 days", []],
    ["Older", []],
  ]);

  [...rooms].sort(compareRooms).forEach((room) => {
    const epoch = activityEpoch(room);
    const label: RoomGroupLabel =
      epoch >= startOfToday
        ? "Today"
        : epoch >= sevenDaysAgo
          ? "Previous 7 days"
          : "Older";
    groups.get(label)?.push(room);
  });

  return [...groups.entries()]
    .filter(([, groupedRooms]) => groupedRooms.length > 0)
    .map(([label, groupedRooms]) => ({ label, rooms: groupedRooms }));
}
