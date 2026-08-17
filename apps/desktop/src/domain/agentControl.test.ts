/** Deterministic tests for revision-aware room agent configuration state. */
import { describe, expect, it } from "vitest";
import {
  applyAgentControlAction,
  createAgentControlState,
  serializeAgentControlDraft,
  validateAgentControlDraft,
} from "./agentControl";
import type {
  AgentCapabilityCatalog,
  AgentControlAction,
  AgentControlState,
  AgentIdentity,
  AgentRosterSnapshot,
  AgentSeatSnapshot,
  RoomBridgeStatus,
} from "./agentControl";

/** Stable signed-in human used by owner-scoped reducer tests. */
const OWNER_ID = "human-owner";

/** Stable second human used by manager authorization tests. */
const OTHER_OWNER_ID = "human-other";

/** Stable server identifier shared by room roster fixtures. */
const SERVER_ID = "server-room-control";

/** Deployment-discovered test catalog with distinct harness and model values. */
const CATALOG: AgentCapabilityCatalog = {
  generation: "catalog-generation-1",
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
        {
          id: "gpt-5.6-terra",
          label: "GPT-5.6 Terra",
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
      ],
      settings: [
        {
          id: "effort",
          label: "Effort",
          required: true,
          control: {
            type: "select",
            options: [{ id: "high", label: "High" }],
          },
        },
      ],
    },
  ],
};

/** Persistent identities available to the room editor. */
const IDENTITIES: readonly AgentIdentity[] = [
  {
    id: "agent-zulu",
    username: "zulu",
    displayName: "Zulu",
    ownerUserId: OWNER_ID,
  },
  {
    id: "agent-alpha",
    username: "alpha",
    displayName: "Alpha",
    ownerUserId: OWNER_ID,
  },
  {
    id: "agent-other",
    username: "other",
    displayName: "Other owner",
    ownerUserId: OTHER_OWNER_ID,
  },
  {
    id: "agent-imported",
    username: "imported",
    displayName: "Imported",
    ownerUserId: null,
  },
];

/** Build one complete server-authoritative room seat. */
function seat(
  seatId: string,
  agentIdentityId: string,
  position: number,
  overrides: Partial<AgentSeatSnapshot> = {},
): AgentSeatSnapshot {
  return {
    seatId,
    agentIdentityId,
    agentUsername: agentIdentityId,
    agentDisplayName: null,
    ownerHumanId: OWNER_ID,
    harnessKey: "codex-cli",
    modelKey: "gpt-5.6-sol",
    settings: {
      effort: "medium",
      turnLimit: 8,
      webSearch: false,
    },
    credentialBindingId: null,
    enabled: true,
    position,
    configurationRevision: 7,
    credentialReadiness: "hostSession",
    runtimeActivation: "active",
    ...overrides,
  };
}

/** Build one complete room roster snapshot with narrow overrides. */
function snapshot(
  overrides: Partial<AgentRosterSnapshot> = {},
): AgentRosterSnapshot {
  return {
    serverId: SERVER_ID,
    desiredRevision: 7,
    activeRevision: 7,
    lastGoodRevision: 7,
    runtimeActivation: "active",
    runtimeErrorCode: null,
    runtimeErrorMessage: null,
    seats: [seat("seat-zulu", "agent-zulu", 1), seat("seat-alpha", "agent-alpha", 0)],
    ...overrides,
  };
}

/** Create one reducer state with deterministic authorization defaults. */
function state(
  overrides: {
    currentHumanId?: string;
    canManageRoom?: boolean;
    identities?: readonly AgentIdentity[];
    catalog?: AgentCapabilityCatalog;
    snapshot?: AgentRosterSnapshot;
  } = {},
): AgentControlState {
  return createAgentControlState({
    currentHumanId: overrides.currentHumanId ?? OWNER_ID,
    canManageRoom: overrides.canManageRoom ?? true,
    identities: overrides.identities ?? IDENTITIES,
    catalog: overrides.catalog ?? CATALOG,
    snapshot: overrides.snapshot ?? snapshot(),
  });
}

/** Apply a deterministic action sequence to one immutable state. */
function reduce(
  initial: AgentControlState,
  actions: readonly AgentControlAction[],
): AgentControlState {
  return actions.reduce(applyAgentControlAction, initial);
}

describe("createAgentControlState", () => {
  it("initializes separate authoritative and position-ordered draft branches", () => {
    const source = snapshot();
    const created = state({ snapshot: source });

    expect(created.serverSnapshot).not.toBe(source);
    expect(created.draft).not.toBe(source.seats);
    expect(created.draft.map((entry) => entry.agentIdentityId)).toEqual([
      "agent-alpha",
      "agent-zulu",
    ]);
    expect(created.draft.map((entry) => entry.position)).toEqual([0, 1]);
    expect(created.dirty).toBe(false);
    expect(created.revisionConflict).toBeNull();
    expect(source.seats.map((entry) => entry.position)).toEqual([1, 0]);
  });

  it("orders tied positions by identity name and stable identifiers", () => {
    const created = state({
      snapshot: snapshot({
        seats: [
          seat("seat-zulu", "agent-zulu", 0),
          seat("seat-alpha", "agent-alpha", 0),
          seat("seat-imported", "missing-identity", 0),
        ],
      }),
    });

    expect(created.draft.map((entry) => entry.agentIdentityId)).toEqual([
      "agent-alpha",
      "missing-identity",
      "agent-zulu",
    ]);
    expect(created.draft.map((entry) => entry.position)).toEqual([0, 0, 0]);
  });
});

describe("applyAgentControlAction", () => {
  it("keeps harness, model, settings, credential, and enabled edits distinct", () => {
    const initial = state();
    const changed = reduce(initial, [
      { type: "setHarness", seatId: "seat-alpha", harnessKey: "claude-code" },
      { type: "setModel", seatId: "seat-alpha", modelKey: "claude-opus" },
      { type: "setSetting", seatId: "seat-alpha", settingId: "effort", value: "high" },
      {
        type: "setCredentialBinding",
        seatId: "seat-alpha",
        credentialBindingId: "binding-alpha",
      },
      { type: "setEnabled", seatId: "seat-alpha", enabled: false },
    ]);
    const edited = changed.draft.find((entry) => entry.seatId === "seat-alpha");

    expect(edited).toMatchObject({
      harnessKey: "claude-code",
      modelKey: "claude-opus",
      settings: { effort: "high" },
      credentialBindingId: "binding-alpha",
      enabled: false,
    });
    expect(changed.dirty).toBe(true);
    expect(initial.draft.find((entry) => entry.seatId === "seat-alpha")).toMatchObject({
      harnessKey: "codex-cli",
      modelKey: "gpt-5.6-sol",
      enabled: true,
    });
  });

  it("computes dirty from semantic values and discards back to server truth", () => {
    const initial = state();
    const semanticallyEqual = applyAgentControlAction(initial, {
      type: "setSetting",
      seatId: "seat-alpha",
      settingId: "effort",
      value: "medium",
    });
    const changed = applyAgentControlAction(semanticallyEqual, {
      type: "setModel",
      seatId: "seat-alpha",
      modelKey: "gpt-5.6-terra",
    });
    const discarded = applyAgentControlAction(changed, { type: "discard" });

    expect(semanticallyEqual.dirty).toBe(false);
    expect(changed.dirty).toBe(true);
    expect(discarded.dirty).toBe(false);
    expect(discarded.revisionConflict).toBeNull();
    expect(discarded.draft).toEqual(initial.draft);
  });

  it("adds only an owned identity and removes its seat without deleting identity state", () => {
    const initial = state({
      snapshot: snapshot({ seats: [seat("seat-alpha", "agent-alpha", 0)] }),
    });
    const added = applyAgentControlAction(initial, {
      type: "addSeat",
      seatId: "seat-new-zulu",
      agentIdentityId: "agent-zulu",
    });
    const denied = applyAgentControlAction(added, {
      type: "addSeat",
      seatId: "seat-denied",
      agentIdentityId: "agent-other",
    });
    const removed = applyAgentControlAction(denied, {
      type: "removeSeat",
      seatId: "seat-new-zulu",
    });

    expect(added.draft.at(-1)).toMatchObject({
      seatId: "seat-new-zulu",
      agentIdentityId: "agent-zulu",
      harnessKey: "",
      modelKey: "",
      settings: {},
      enabled: true,
      position: 1,
    });
    expect(denied).toBe(added);
    expect(removed.draft).toHaveLength(1);
    expect(removed.identities).toContainEqual(
      expect.objectContaining({ id: "agent-zulu" }),
    );
  });

  it("allows manager disable and removal but rejects reconfiguration or enabling", () => {
    const managerState = state({
      currentHumanId: OWNER_ID,
      canManageRoom: true,
      snapshot: snapshot({
        seats: [
          seat("seat-other", "agent-other", 0, {
            ownerHumanId: OTHER_OWNER_ID,
          }),
        ],
      }),
    });
    const reconfigured = applyAgentControlAction(managerState, {
      type: "setModel",
      seatId: "seat-other",
      modelKey: "gpt-5.6-terra",
    });
    const disabled = applyAgentControlAction(managerState, {
      type: "setEnabled",
      seatId: "seat-other",
      enabled: false,
    });
    const removed = applyAgentControlAction(managerState, {
      type: "removeSeat",
      seatId: "seat-other",
    });
    const initiallyDisabled = state({
      currentHumanId: OWNER_ID,
      canManageRoom: true,
      snapshot: snapshot({
        seats: [
          seat("seat-other", "agent-other", 0, {
            ownerHumanId: OTHER_OWNER_ID,
            enabled: false,
          }),
        ],
      }),
    });
    const enabled = applyAgentControlAction(initiallyDisabled, {
      type: "setEnabled",
      seatId: "seat-other",
      enabled: true,
    });

    expect(reconfigured).toBe(managerState);
    expect(disabled.draft[0].enabled).toBe(false);
    expect(removed.draft).toEqual([]);
    expect(enabled).toBe(initiallyDisabled);
  });

  it("requires manager authority for reorder while owners may remove their own seats", () => {
    const ownerState = state({ canManageRoom: false });
    const reordered = applyAgentControlAction(ownerState, {
      type: "moveSeat",
      seatId: "seat-zulu",
      toIndex: 0,
    });
    const removed = applyAgentControlAction(ownerState, {
      type: "removeSeat",
      seatId: "seat-alpha",
    });
    const managerState = state({ canManageRoom: true });
    const managerReordered = applyAgentControlAction(managerState, {
      type: "moveSeat",
      seatId: "seat-zulu",
      toIndex: 0,
    });

    expect(reordered).toBe(ownerState);
    expect(removed.draft.map((entry) => entry.seatId)).toEqual(["seat-zulu"]);
    expect(removed.draft[0].position).toBe(1);
    expect(validateAgentControlDraft(removed).map((issue) => issue.code)).not.toContain(
      "manager_required",
    );
    expect(serializeAgentControlDraft(removed).ok).toBe(true);
    expect(managerReordered.draft.map((entry) => entry.seatId)).toEqual([
      "seat-zulu",
      "seat-alpha",
    ]);
    expect(managerReordered.draft.map((entry) => entry.position)).toEqual([0, 1]);
  });

  it("clears harness-specific model, settings, and binding after a harness change", () => {
    const changed = applyAgentControlAction(state(), {
      type: "setHarness",
      seatId: "seat-alpha",
      harnessKey: "claude-code",
    });

    expect(changed.draft.find((entry) => entry.seatId === "seat-alpha")).toMatchObject({
      harnessKey: "claude-code",
      modelKey: "",
      settings: {},
      credentialBindingId: null,
    });
  });

  it("adopts apply success and preserves a dirty draft across revision conflict", () => {
    const edited = applyAgentControlAction(state(), {
      type: "setModel",
      seatId: "seat-alpha",
      modelKey: "gpt-5.6-terra",
    });
    const pendingSnapshot = snapshot({
      desiredRevision: 8,
      runtimeActivation: "pending",
      seats: edited.draft.map((entry) =>
        seat(entry.seatId, entry.agentIdentityId, entry.position, {
          ...entry,
          configurationRevision: 8,
          runtimeActivation: "pending",
        }),
      ),
    });
    const applied = applyAgentControlAction(edited, {
      type: "applySucceeded",
      snapshot: pendingSnapshot,
    });
    const editedAgain = applyAgentControlAction(applied, {
      type: "setModel",
      seatId: "seat-alpha",
      modelKey: "gpt-5.6-sol",
    });
    const remoteSnapshot = snapshot({
      desiredRevision: 9,
      seats: snapshot().seats.map((entry) =>
        entry.seatId === "seat-alpha"
          ? { ...entry, modelKey: "gpt-5.6-terra", configurationRevision: 9 }
          : { ...entry, configurationRevision: 9 },
      ),
    });
    const conflicted = applyAgentControlAction(editedAgain, {
      type: "revisionConflict",
      snapshot: remoteSnapshot,
    });

    expect(applied.dirty).toBe(false);
    expect(applied.serverSnapshot.desiredRevision).toBe(8);
    expect(applied.serverSnapshot.runtimeActivation).toBe("pending");
    expect(conflicted.draft).toEqual(editedAgain.draft);
    expect(conflicted.serverSnapshot.desiredRevision).toBe(9);
    expect(conflicted.revisionConflict).toEqual({
      attemptedRevision: 8,
      serverRevision: 9,
    });
    expect(conflicted.dirty).toBe(true);
    expect(serializeAgentControlDraft(conflicted)).toMatchObject({
      ok: true,
      request: { expectedRevision: 9 },
    });
  });

  it("projects pending, active, and failed runtime status without replacing the draft", () => {
    const edited = applyAgentControlAction(state(), {
      type: "setModel",
      seatId: "seat-alpha",
      modelKey: "gpt-5.6-terra",
    });
    const statuses: RoomBridgeStatus[] = [
      {
        paused: false,
        desiredRevision: 8,
        activeRevision: 7,
        lastGoodRevision: 7,
        runtimeActivation: "pending",
        runtimeErrorCode: null,
        runtimeErrorMessage: null,
      },
      {
        paused: false,
        desiredRevision: 8,
        activeRevision: 8,
        lastGoodRevision: 8,
        runtimeActivation: "active",
        runtimeErrorCode: null,
        runtimeErrorMessage: null,
      },
      {
        paused: false,
        desiredRevision: 9,
        activeRevision: 8,
        lastGoodRevision: 8,
        runtimeActivation: "failed",
        runtimeErrorCode: "bridge_start_failed",
        runtimeErrorMessage: "The managed room bridge could not start or remain available.",
      },
    ];
    const pending = applyAgentControlAction(edited, {
      type: "runtimeUpdated",
      status: statuses[0],
    });
    const active = applyAgentControlAction(pending, {
      type: "runtimeUpdated",
      status: statuses[1],
    });
    const failed = applyAgentControlAction(active, {
      type: "runtimeUpdated",
      status: statuses[2],
    });

    expect(failed.draft).toEqual(edited.draft);
    expect(pending.serverSnapshot.runtimeActivation).toBe("pending");
    expect(active.serverSnapshot.runtimeActivation).toBe("active");
    expect(failed.serverSnapshot).toMatchObject({
      desiredRevision: 9,
      activeRevision: 8,
      lastGoodRevision: 8,
      runtimeActivation: "failed",
      runtimeErrorCode: "bridge_start_failed",
    });
    expect(failed.serverSnapshot.seats.every((entry) => entry.runtimeActivation === "failed")).toBe(
      true,
    );
    expect(failed.dirty).toBe(true);
  });
});

describe("validation and serialization", () => {
  it("rejects reassigning a stable server seat to a different agent identity", () => {
    const initial = state({
      snapshot: snapshot({ seats: [seat("seat-alpha", "agent-alpha", 0)] }),
    });
    const forged: AgentControlState = {
      ...initial,
      draft: initial.draft.map((entry) => ({
        ...entry,
        agentIdentityId: "agent-zulu",
      })),
    };

    expect(validateAgentControlDraft(forged).map((issue) => issue.code)).toContain(
      "seat_identity_changed",
    );
  });

  it("validates catalog-defined models, settings, and required credential bindings", () => {
    const harnessChanged = applyAgentControlAction(state(), {
      type: "setHarness",
      seatId: "seat-alpha",
      harnessKey: "claude-code",
    });
    const incompleteIssues = validateAgentControlDraft(harnessChanged);
    const modelSelected = applyAgentControlAction(harnessChanged, {
      type: "setModel",
      seatId: "seat-alpha",
      modelKey: "claude-opus",
    });
    const settingSelected = applyAgentControlAction(modelSelected, {
      type: "setSetting",
      seatId: "seat-alpha",
      settingId: "effort",
      value: "high",
    });
    const complete = applyAgentControlAction(settingSelected, {
      type: "setCredentialBinding",
      seatId: "seat-alpha",
      credentialBindingId: "binding-alpha",
    });

    expect(incompleteIssues.map((issue) => issue.code)).toEqual(
      expect.arrayContaining(["model_required", "setting_required", "credential_required"]),
    );
    expect(validateAgentControlDraft(complete)).toEqual([]);
  });

  it("reports selections invalidated by a new catalog generation", () => {
    const nextCatalog: AgentCapabilityCatalog = {
      ...CATALOG,
      generation: "catalog-generation-2",
      harnesses: CATALOG.harnesses.map((harness) =>
        harness.id === "codex-cli"
          ? {
              ...harness,
              models: harness.models.filter((model) => model.id !== "gpt-5.6-sol"),
            }
          : harness,
      ),
    };
    const refreshed = applyAgentControlAction(state(), {
      type: "catalogUpdated",
      catalog: nextCatalog,
    });

    expect(refreshed.catalog.generation).toBe("catalog-generation-2");
    expect(validateAgentControlDraft(refreshed).map((issue) => issue.code)).toContain(
      "model_unavailable",
    );
  });

  it("serializes one complete position-ordered roster with the observed revision", () => {
    const reordered = applyAgentControlAction(state(), {
      type: "moveSeat",
      seatId: "seat-zulu",
      toIndex: 0,
    });
    const serialized = serializeAgentControlDraft(reordered);

    expect(serialized).toEqual({
      ok: true,
      request: {
        expectedRevision: 7,
        seats: [
          expect.objectContaining({
            seatId: "seat-zulu",
            agentIdentityId: "agent-zulu",
            position: 0,
          }),
          expect.objectContaining({
            seatId: "seat-alpha",
            agentIdentityId: "agent-alpha",
            position: 1,
          }),
        ],
      },
    });
  });
});
