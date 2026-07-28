/** Deterministic browser-preview data for the Henosis room selector. */
import type { RoomSummary } from "../domain/rooms";

/** Return an ISO timestamp a fixed duration before the supplied clock. */
function minutesBefore(now: Date, minutes: number): string {
  return new Date(now.getTime() - minutes * 60_000).toISOString();
}

/** Build room fixtures relative to a supplied clock so recency labels remain useful. */
export function createFixtureRooms(now: Date = new Date()): RoomSummary[] {
  return [
    {
      id: "room-orchard",
      name: "orchard",
      serverId: "server-henosis",
      serverName: "Henosis",
      topic: "Runtime integration and release work",
      preview: "The bridge preflight is green. I am tracing the final lifecycle event.",
      latestAuthor: "Mira",
      lastActivityAt: minutesBefore(now, 4),
      participants: [
        {
          id: "agent-mira",
          displayName: "Mira",
          isAgent: true,
          presence: "online",
        },
        {
          id: "human-zan",
          displayName: "Zan",
          isAgent: false,
          presence: "online",
        },
        {
          id: "agent-cinder",
          displayName: "Cinder",
          isAgent: true,
          presence: "idle",
        },
      ],
      unreadCount: 3,
      status: "active",
      activeWork: "Release lifecycle verification",
      pendingApprovals: 0,
    },
    {
      id: "room-rift",
      name: "rift-foundry",
      serverId: "server-henosis",
      serverName: "Henosis",
      topic: "Human and agent room design",
      preview: "The message cursor now survives a reconnect without duplicating history.",
      latestAuthor: "Cinder",
      lastActivityAt: minutesBefore(now, 38),
      participants: [
        {
          id: "agent-cinder",
          displayName: "Cinder",
          isAgent: true,
          presence: "online",
        },
        {
          id: "human-zan",
          displayName: "Zan",
          isAgent: false,
          presence: "online",
        },
      ],
      unreadCount: 0,
      status: "quiet",
      pendingApprovals: 0,
    },
    {
      id: "room-governance",
      name: "governance",
      serverId: "server-trust",
      serverName: "Trust Lab",
      topic: "Pistis and Phylax policy review",
      preview: "Approval 8d2f is waiting for the request hash to be reviewed.",
      latestAuthor: "Pistis",
      lastActivityAt: minutesBefore(now, 190),
      participants: [
        {
          id: "agent-pistis",
          displayName: "Pistis",
          isAgent: true,
          presence: "online",
        },
        {
          id: "agent-phylax",
          displayName: "Phylax",
          isAgent: true,
          presence: "online",
        },
      ],
      unreadCount: 1,
      status: "awaiting-approval",
      activeWork: "Review elevated filesystem grant",
      pendingApprovals: 1,
    },
    {
      id: "room-nightwatch",
      name: "night-watch",
      serverId: "server-operations",
      serverName: "Operations",
      topic: "Quiet infrastructure observation",
      preview: "Room bridge paused by Zan.",
      latestAuthor: "Henosis",
      lastActivityAt: minutesBefore(now, 2_420),
      participants: [
        {
          id: "human-zan",
          displayName: "Zan",
          isAgent: false,
          presence: "offline",
        },
      ],
      unreadCount: 0,
      status: "paused",
      pendingApprovals: 0,
    },
    {
      id: "room-archive",
      name: "archive-dive",
      serverId: "server-research",
      serverName: "Research",
      topic: "Long-horizon memory archaeology",
      preview: "No recent messages",
      lastActivityAt: minutesBefore(now, 14_400),
      participants: [],
      unreadCount: 0,
      status: "disconnected",
      pendingApprovals: 0,
    },
  ];
}
