/** Deterministic browser-preview data for the Henosis room selector. */
import type {
  AgentCapabilityCatalog,
  AgentIdentity,
  AgentRosterSnapshot,
} from "../domain/agentControl";
import type { RoomSummary } from "../domain/rooms";

/** Return an ISO timestamp a fixed duration before the supplied clock. */
function minutesBefore(now: Date, minutes: number): string {
  return new Date(now.getTime() - minutes * 60_000).toISOString();
}

/** Build a fresh deployment-style capability catalog for dashboard fixtures. */
export function createFixtureAgentCatalog(): AgentCapabilityCatalog {
  return {
    generation: "fixture-catalog-1",
    harnesses: [
      {
        id: "codex-cli",
        label: "Codex CLI",
        available: true,
        unavailableReason: null,
        credentialMode: "hostSession",
        models: [
          {
            id: "gpt-5.6-sol",
            label: "GPT-5.6 Sol",
            available: true,
            unavailableReason: null,
          },
        ],
        settings: [
          {
            id: "effort",
            label: "Reasoning effort",
            required: true,
            control: {
              type: "select",
              options: [
                { id: "medium", label: "Medium" },
                { id: "high", label: "High" },
              ],
            },
          },
          {
            id: "turnLimit",
            label: "Turn limit",
            required: false,
            control: {
              type: "integer",
              minimum: 1,
              maximum: 20,
              step: 1,
            },
          },
          {
            id: "webSearch",
            label: "Web search",
            required: false,
            control: { type: "boolean" },
          },
        ],
      },
      {
        id: "claude-code",
        label: "Claude Code",
        available: true,
        unavailableReason: null,
        credentialMode: "requiredBinding",
        models: [
          {
            id: "claude-opus",
            label: "Claude Opus",
            available: true,
            unavailableReason: null,
          },
          {
            id: "claude-sonnet",
            label: "Claude Sonnet",
            available: true,
            unavailableReason: null,
          },
        ],
        settings: [],
      },
    ],
  };
}

/** Build fresh persistent identities spanning every fixture ownership state. */
export function createFixtureAgentIdentities(): AgentIdentity[] {
  return [
    {
      id: "agent-mira",
      username: "mira",
      displayName: "Mira",
      ownerUserId: "fixture-user",
    },
    {
      id: "agent-lumen",
      username: "lumen",
      displayName: "Lumen",
      ownerUserId: "fixture-user",
    },
    {
      id: "agent-cinder",
      username: "cinder",
      displayName: "Cinder",
      ownerUserId: "human-steward",
    },
    {
      id: "agent-imported",
      username: "imported-scout",
      displayName: "Imported scout",
      ownerUserId: null,
    },
  ];
}

/** Build fresh per-server roster snapshots with distinct opaque readiness states. */
export function createFixtureAgentRosters(): AgentRosterSnapshot[] {
  /** Build one empty idle roster for a secondary fixture server. */
  const emptyRoster = (serverId: string): AgentRosterSnapshot => ({
    serverId,
    desiredRevision: null,
    activeRevision: null,
    lastGoodRevision: null,
    runtimeActivation: "idle",
    runtimeErrorCode: null,
    runtimeErrorMessage: null,
    seats: [],
  });

  return [
    {
      serverId: "server-henosis",
      desiredRevision: 2,
      activeRevision: 2,
      lastGoodRevision: 2,
      runtimeActivation: "active",
      runtimeErrorCode: null,
      runtimeErrorMessage: null,
      seats: [
        {
          seatId: "seat-mira",
          agentIdentityId: "agent-mira",
          agentUsername: "mira",
          agentDisplayName: "Mira",
          ownerHumanId: "fixture-user",
          harnessKey: "codex-cli",
          modelKey: "gpt-5.6-sol",
          settings: { effort: "medium", turnLimit: 8, webSearch: false },
          credentialBindingId: null,
          enabled: true,
          position: 0,
          configurationRevision: 2,
          credentialReadiness: "hostSession",
          runtimeActivation: "active",
        },
        {
          seatId: "seat-cinder",
          agentIdentityId: "agent-cinder",
          agentUsername: "cinder",
          agentDisplayName: "Cinder",
          ownerHumanId: "human-steward",
          harnessKey: "claude-code",
          modelKey: "claude-sonnet",
          settings: {},
          credentialBindingId: "binding-cinder",
          enabled: true,
          position: 1,
          configurationRevision: 2,
          credentialReadiness: "ready",
          runtimeActivation: "active",
        },
        {
          seatId: "seat-imported",
          agentIdentityId: "agent-imported",
          agentUsername: "imported-scout",
          agentDisplayName: "Imported scout",
          ownerHumanId: null,
          harnessKey: "claude-code",
          modelKey: "claude-opus",
          settings: {},
          credentialBindingId: "binding-imported",
          enabled: false,
          position: 2,
          configurationRevision: 2,
          credentialReadiness: "attention",
          runtimeActivation: "active",
        },
      ],
    },
    emptyRoster("server-trust"),
    emptyRoster("server-operations"),
    emptyRoster("server-research"),
  ];
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
          id: "human-operator",
          displayName: "Operator",
          isAgent: false,
          presence: "online",
        },
        {
          id: "human-steward",
          displayName: "Rowan",
          isAgent: false,
          presence: "idle",
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
          id: "human-operator",
          displayName: "Operator",
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
      preview: "Room bridge paused by Operator.",
      latestAuthor: "Henosis",
      lastActivityAt: minutesBefore(now, 2_420),
      participants: [
        {
          id: "human-operator",
          displayName: "Operator",
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
