/** Pure, revision-aware room agent configuration state for the Henosis dashboard. */

/** Maximum number of seats accepted by Rift for one room roster revision. */
export const MAX_AGENT_SEATS = 32;

/** Maximum UTF-8 JSON size accepted for one seat's non-secret settings. */
export const MAX_AGENT_SETTINGS_BYTES = 16 * 1024;

/** Persistent agent identity owned by one Rift human. */
export interface OwnedAgentIdentity {
  /** Stable Rift user identifier for the agent. */
  readonly id: string;
  /** Unique Rift username. */
  readonly username: string;
  /** Optional human-facing identity name. */
  readonly displayName: string | null;
  /** Stable Rift user identifier for the owning human. */
  readonly ownerUserId: string;
}

/** Imported persistent agent identity that has not been claimed. */
export interface UnownedAgentIdentity {
  /** Stable Rift user identifier for the agent. */
  readonly id: string;
  /** Unique Rift username. */
  readonly username: string;
  /** Optional human-facing identity name. */
  readonly displayName: string | null;
  /** Explicit absence of an owner until a human claims the identity. */
  readonly ownerUserId: null;
}

/** Any persistent agent identity visible to the room dashboard. */
export type AgentIdentity = OwnedAgentIdentity | UnownedAgentIdentity;

/** Credential selection behavior declared by one execution harness. */
export type HarnessCredentialMode =
  | "hostSession"
  | "optionalBinding"
  | "requiredBinding";

/** One selectable model discovered beneath an execution harness. */
export interface ModelCapability {
  /** Stable model identifier submitted to Rift. */
  readonly id: string;
  /** Human-facing model label. */
  readonly label: string;
  /** Whether the connected deployment can currently use the model. */
  readonly available: boolean;
  /** Safe deployment-supplied explanation when unavailable. */
  readonly unavailableReason: string | null;
}

/** One selectable value for a catalog-defined setting. */
export interface SettingCapabilityOption {
  /** Stable non-secret value submitted to Rift. */
  readonly id: string;
  /** Human-facing option label. */
  readonly label: string;
}

/** Typed control metadata for one catalog-defined setting. */
export type SettingCapabilityControl =
  | {
      /** Discriminator for a finite string choice. */
      readonly type: "select";
      /** Allowed values in deployment-supplied display order. */
      readonly options: readonly SettingCapabilityOption[];
    }
  | {
      /** Discriminator for a bounded stepped integer. */
      readonly type: "integer";
      /** Inclusive lower bound. */
      readonly minimum: number;
      /** Inclusive upper bound. */
      readonly maximum: number;
      /** Positive increment measured from the lower bound. */
      readonly step: number;
    }
  | {
      /** Discriminator for a boolean toggle. */
      readonly type: "boolean";
    };

/** One typed non-secret setting exposed by a host harness. */
export interface SettingCapability {
  /** Stable key used in a seat settings object. */
  readonly id: string;
  /** Human-facing setting label. */
  readonly label: string;
  /** Whether every seat using the harness must submit a value. */
  readonly required: boolean;
  /** Control type and validation constraints. */
  readonly control: SettingCapabilityControl;
}

/** One deployment-discovered execution harness and its choices. */
export interface HarnessCapability {
  /** Stable harness identifier submitted to Rift. */
  readonly id: string;
  /** Human-facing harness label. */
  readonly label: string;
  /** Whether the connected deployment can currently use the harness. */
  readonly available: boolean;
  /** Safe deployment-supplied explanation when unavailable. */
  readonly unavailableReason: string | null;
  /** Supported credential selection behavior. */
  readonly credentialMode: HarnessCredentialMode;
  /** Models currently declared beneath this harness. */
  readonly models: readonly ModelCapability[];
  /** Typed non-secret settings currently declared by this harness. */
  readonly settings: readonly SettingCapability[];
}

/** Generation-stamped execution catalog discovered from the connected host. */
export interface AgentCapabilityCatalog {
  /** Opaque generation changed whenever host discovery is rebuilt. */
  readonly generation: string;
  /** Deployment-discovered harnesses without a React allowlist. */
  readonly harnesses: readonly HarnessCapability[];
}

/** Opaque credential readiness safe to expose to React. */
export type AgentCredentialReadiness =
  | "hostSession"
  | "ready"
  | "unavailable"
  | "attention";

/** Durable activation state for a desired room roster revision. */
export type RuntimeActivationState = "idle" | "pending" | "active" | "failed";

/** One server-authoritative room agent seat. */
export interface AgentSeatSnapshot {
  /** Stable seat identifier retained across reordering. */
  readonly seatId: string;
  /** Persistent Rift agent identity occupying the seat. */
  readonly agentIdentityId: string;
  /** Unique public username of the selected agent identity. */
  readonly agentUsername: string;
  /** Optional human-facing name of the selected agent identity. */
  readonly agentDisplayName: string | null;
  /** Current human owner, or null for an unclaimed imported identity. */
  readonly ownerHumanId: string | null;
  /** Deployment-discovered execution harness key. */
  readonly harnessKey: string;
  /** Deployment-discovered model key beneath the harness. */
  readonly modelKey: string;
  /** Typed non-secret settings validated against the harness catalog. */
  readonly settings: Readonly<Record<string, unknown>>;
  /** Opaque deployment-owned credential binding identifier. */
  readonly credentialBindingId: string | null;
  /** Whether this seat participates in room responses. */
  readonly enabled: boolean;
  /** Non-negative room display and execution order. */
  readonly position: number;
  /** Desired immutable revision containing this seat. */
  readonly configurationRevision: number | null;
  /** Credential usability without credential contents or host locators. */
  readonly credentialReadiness: AgentCredentialReadiness;
  /** Current activation state of the containing roster revision. */
  readonly runtimeActivation: RuntimeActivationState;
}

/** Complete server-authoritative room roster and activation state. */
export interface AgentRosterSnapshot {
  /** Rift server whose bridge the roster configures. */
  readonly serverId: string;
  /** Latest desired immutable revision. */
  readonly desiredRevision: number | null;
  /** Revision currently running in the bridge. */
  readonly activeRevision: number | null;
  /** Most recent revision proven to start successfully. */
  readonly lastGoodRevision: number | null;
  /** Current asynchronous activation state. */
  readonly runtimeActivation: RuntimeActivationState;
  /** Stable activation failure code when the desired revision failed. */
  readonly runtimeErrorCode: string | null;
  /** Bounded safe activation failure detail. */
  readonly runtimeErrorMessage: string | null;
  /** Desired seats in server-authoritative order. */
  readonly seats: readonly AgentSeatSnapshot[];
}

/** One editable non-secret seat in a complete roster replacement. */
export interface AgentSeatDraft {
  /** Stable seat identifier retained across edits and reordering. */
  readonly seatId: string;
  /** Persistent Rift agent identity assigned to the seat. */
  readonly agentIdentityId: string;
  /** Deployment-discovered execution harness key. */
  readonly harnessKey: string;
  /** Deployment-discovered model key beneath the harness. */
  readonly modelKey: string;
  /** Typed non-secret settings validated against the live catalog. */
  readonly settings: Readonly<Record<string, unknown>>;
  /** Opaque deployment-owned credential binding identifier. */
  readonly credentialBindingId: string | null;
  /** Whether this seat participates in room responses. */
  readonly enabled: boolean;
  /** Server position, changed only by explicit manager-authorized reordering. */
  readonly position: number;
}

/** Optimistic complete roster replacement submitted through Tauri. */
export interface ApplyAgentRosterRequest {
  /** Desired revision observed before the local draft was edited. */
  readonly expectedRevision: number | null;
  /** Complete next roster in stable position order. */
  readonly seats: readonly AgentSeatDraft[];
}

/** Public pause and activation state for one room bridge. */
export interface RoomBridgeStatus {
  /** Whether autonomous bridge activity is paused. */
  readonly paused: boolean;
  /** Latest desired immutable revision. */
  readonly desiredRevision: number | null;
  /** Revision currently running in the bridge. */
  readonly activeRevision: number | null;
  /** Most recent revision proven to start successfully. */
  readonly lastGoodRevision: number | null;
  /** Current asynchronous activation state. */
  readonly runtimeActivation: RuntimeActivationState;
  /** Stable activation failure code when the desired revision failed. */
  readonly runtimeErrorCode: string | null;
  /** Bounded safe activation failure detail. */
  readonly runtimeErrorMessage: string | null;
}

/** Revision mismatch retained beside the user's unsaved draft. */
export interface AgentRevisionConflict {
  /** Revision the editor originally observed. */
  readonly attemptedRevision: number | null;
  /** Current desired revision returned after the conflict. */
  readonly serverRevision: number | null;
}

/** Complete immutable React state for room agent configuration. */
export interface AgentControlState {
  /** Signed-in Rift human used for owner checks. */
  readonly currentHumanId: string;
  /** Whether the signed-in human may manage room participation and order. */
  readonly canManageRoom: boolean;
  /** Persistent identities known to the dashboard independent of room seats. */
  readonly identities: readonly AgentIdentity[];
  /** Latest deployment-discovered execution catalog. */
  readonly catalog: AgentCapabilityCatalog;
  /** Latest server-authoritative roster, never edited in place. */
  readonly serverSnapshot: AgentRosterSnapshot;
  /** Latest room bridge pause and activation projection. */
  readonly bridgeStatus: RoomBridgeStatus;
  /** Position-ordered local roster edits. */
  readonly draft: readonly AgentSeatDraft[];
  /** Whether ordered draft semantics differ from server truth. */
  readonly dirty: boolean;
  /** Latest stale-revision context while a local draft remains visible. */
  readonly revisionConflict: AgentRevisionConflict | null;
}

/** Inputs required to initialize one room agent control reducer. */
export interface AgentControlInitialization {
  /** Signed-in Rift human used for owner checks. */
  readonly currentHumanId: string;
  /** Whether the signed-in human may manage room participation and order. */
  readonly canManageRoom: boolean;
  /** Persistent identities known to the dashboard. */
  readonly identities: readonly AgentIdentity[];
  /** Current deployment-discovered execution catalog. */
  readonly catalog: AgentCapabilityCatalog;
  /** Current server-authoritative room roster. */
  readonly snapshot: AgentRosterSnapshot;
}

/** Primitive setting value accepted by every alpha catalog control. */
export type AgentSettingValue = string | number | boolean;

/** Every pure state transition accepted by the room agent reducer. */
export type AgentControlAction =
  | {
      /** Change one owner-controlled execution harness. */
      readonly type: "setHarness";
      /** Stable seat being edited. */
      readonly seatId: string;
      /** Deployment-discovered replacement harness key. */
      readonly harnessKey: string;
    }
  | {
      /** Change one owner-controlled model independently from its harness. */
      readonly type: "setModel";
      /** Stable seat being edited. */
      readonly seatId: string;
      /** Deployment-discovered replacement model key. */
      readonly modelKey: string;
    }
  | {
      /** Set one catalog-defined non-secret setting. */
      readonly type: "setSetting";
      /** Stable seat being edited. */
      readonly seatId: string;
      /** Catalog-defined settings-object key. */
      readonly settingId: string;
      /** Primitive value validated against the current harness. */
      readonly value: AgentSettingValue;
    }
  | {
      /** Remove one setting so validation can expose a missing required value. */
      readonly type: "removeSetting";
      /** Stable seat being edited. */
      readonly seatId: string;
      /** Catalog-defined settings-object key. */
      readonly settingId: string;
    }
  | {
      /** Select or clear an opaque deployment-owned credential binding. */
      readonly type: "setCredentialBinding";
      /** Stable seat being edited. */
      readonly seatId: string;
      /** Opaque binding identifier or null for host-session behavior. */
      readonly credentialBindingId: string | null;
    }
  | {
      /** Enable an owned seat or safely disable a managed seat. */
      readonly type: "setEnabled";
      /** Stable seat being edited. */
      readonly seatId: string;
      /** Desired participation state. */
      readonly enabled: boolean;
    }
  | {
      /** Add one owned persistent identity to the room. */
      readonly type: "addSeat";
      /** Caller-generated stable opaque seat identifier. */
      readonly seatId: string;
      /** Owned persistent identity assigned to the new seat. */
      readonly agentIdentityId: string;
    }
  | {
      /** Remove one owned or manager-controlled seat from this room only. */
      readonly type: "removeSeat";
      /** Stable seat being removed. */
      readonly seatId: string;
    }
  | {
      /** Reorder one seat under room-manager authority. */
      readonly type: "moveSeat";
      /** Stable seat being moved. */
      readonly seatId: string;
      /** Zero-based destination after removal from the current order. */
      readonly toIndex: number;
    }
  | {
      /** Restore the local draft from current server truth. */
      readonly type: "discard";
    }
  | {
      /** Adopt an authoritative roster returned after a successful apply. */
      readonly type: "applySucceeded";
      /** Complete returned roster, commonly in pending activation state. */
      readonly snapshot: AgentRosterSnapshot;
    }
  | {
      /** Retain the local draft beside fresher server truth after HTTP 409. */
      readonly type: "revisionConflict";
      /** Complete current roster fetched after the conflict. */
      readonly snapshot: AgentRosterSnapshot;
    }
  | {
      /** Project a bridge lifecycle refresh without replacing roster edits. */
      readonly type: "runtimeUpdated";
      /** Latest sanitized bridge lifecycle status. */
      readonly status: RoomBridgeStatus;
    }
  | {
      /** Replace capability metadata while preserving the current draft. */
      readonly type: "catalogUpdated";
      /** Latest generation-stamped deployment catalog. */
      readonly catalog: AgentCapabilityCatalog;
    };

/** Stable validation codes rendered without parsing human-readable messages. */
export type AgentControlValidationCode =
  | "too_many_seats"
  | "duplicate_seat"
  | "duplicate_agent"
  | "duplicate_position"
  | "invalid_position"
  | "harness_required"
  | "harness_unavailable"
  | "model_required"
  | "model_unavailable"
  | "unknown_setting"
  | "setting_required"
  | "setting_invalid"
  | "settings_too_large"
  | "credential_required"
  | "credential_not_ready"
  | "owner_required"
  | "manager_required"
  | "seat_identity_changed";

/** One actionable validation problem attached to a seat or whole roster. */
export interface AgentControlValidationIssue {
  /** Stable machine-readable issue code. */
  readonly code: AgentControlValidationCode;
  /** Affected seat, or null for a roster-wide problem. */
  readonly seatId: string | null;
  /** Affected field when a control can focus it directly. */
  readonly field: string | null;
  /** Safe human-readable recovery guidance. */
  readonly message: string;
}

/** Successful complete roster serialization. */
export interface AgentControlSerializationSuccess {
  /** Success discriminator. */
  readonly ok: true;
  /** Complete revision-checked roster request. */
  readonly request: ApplyAgentRosterRequest;
}

/** Failed serialization retaining every actionable validation issue. */
export interface AgentControlSerializationFailure {
  /** Failure discriminator. */
  readonly ok: false;
  /** Validation issues that must be resolved before apply. */
  readonly issues: readonly AgentControlValidationIssue[];
}

/** Result of converting local state into a complete roster request. */
export type AgentControlSerializationResult =
  | AgentControlSerializationSuccess
  | AgentControlSerializationFailure;

/** Clone one settings object without sharing its mutable top-level container. */
function cloneSettings(settings: Readonly<Record<string, unknown>>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(settings));
}

/** Clone one editable seat. */
function cloneDraftSeat(seat: AgentSeatDraft): AgentSeatDraft {
  return { ...seat, settings: cloneSettings(seat.settings) };
}

/** Convert one authoritative seat into its editable secret-free shape. */
function draftFromSnapshot(seat: AgentSeatSnapshot): AgentSeatDraft {
  return {
    seatId: seat.seatId,
    agentIdentityId: seat.agentIdentityId,
    harnessKey: seat.harnessKey,
    modelKey: seat.modelKey,
    settings: cloneSettings(seat.settings),
    credentialBindingId: seat.credentialBindingId,
    enabled: seat.enabled,
    position: seat.position,
  };
}

/** Clone one complete server snapshot without retaining mutable array or setting containers. */
function cloneSnapshot(snapshot: AgentRosterSnapshot): AgentRosterSnapshot {
  return {
    ...snapshot,
    seats: snapshot.seats.map((seat) => ({
      ...seat,
      settings: cloneSettings(seat.settings),
    })),
  };
}

/** Clone one deployment capability catalog into reducer-owned containers. */
function cloneCatalog(catalog: AgentCapabilityCatalog): AgentCapabilityCatalog {
  return {
    generation: catalog.generation,
    harnesses: catalog.harnesses.map((harness) => ({
      ...harness,
      models: harness.models.map((model) => ({ ...model })),
      settings: harness.settings.map((setting) => ({
        ...setting,
        control:
          setting.control.type === "select"
            ? {
                type: "select",
                options: setting.control.options.map((option) => ({ ...option })),
              }
            : { ...setting.control },
      })),
    })),
  };
}

/** Return a stable human-facing identity key with an opaque fallback. */
function identitySortKey(
  identityId: string,
  identities: readonly AgentIdentity[],
): string {
  const identity = identities.find((candidate) => candidate.id === identityId);
  return identity?.displayName?.trim() || identity?.username || identityId;
}

/** Compare seat positions, identity names, and stable identifiers deterministically. */
function compareDraftSeats(
  left: AgentSeatDraft,
  right: AgentSeatDraft,
  identities: readonly AgentIdentity[],
): number {
  const positionDifference = left.position - right.position;
  if (positionDifference !== 0) {
    return positionDifference;
  }
  const identityDifference = identitySortKey(left.agentIdentityId, identities).localeCompare(
    identitySortKey(right.agentIdentityId, identities),
    undefined,
    { sensitivity: "base" },
  );
  if (identityDifference !== 0) {
    return identityDifference;
  }
  const agentDifference = left.agentIdentityId.localeCompare(right.agentIdentityId);
  return agentDifference !== 0 ? agentDifference : left.seatId.localeCompare(right.seatId);
}

/** Sort a complete draft without mutating seats or changing server positions. */
function orderDraft(
  draft: readonly AgentSeatDraft[],
  identities: readonly AgentIdentity[],
): AgentSeatDraft[] {
  return draft
    .map(cloneDraftSeat)
    .sort((left, right) => compareDraftSeats(left, right, identities));
}

/** Canonicalize settings key order for semantic equality checks. */
function canonicalSettings(settings: Readonly<Record<string, unknown>>): string {
  return JSON.stringify(
    Object.entries(settings).sort(([left], [right]) => left.localeCompare(right)),
  );
}

/** Test whether two editable seats carry the same submitted semantics. */
function seatsAreSemanticallyEqual(left: AgentSeatDraft, right: AgentSeatDraft): boolean {
  return (
    left.seatId === right.seatId &&
    left.agentIdentityId === right.agentIdentityId &&
    left.harnessKey === right.harnessKey &&
    left.modelKey === right.modelKey &&
    canonicalSettings(left.settings) === canonicalSettings(right.settings) &&
    left.credentialBindingId === right.credentialBindingId &&
    left.enabled === right.enabled &&
    left.position === right.position
  );
}

/** Compare an ordered local draft with one authoritative server snapshot. */
function draftIsDirty(
  draft: readonly AgentSeatDraft[],
  snapshot: AgentRosterSnapshot,
  identities: readonly AgentIdentity[],
): boolean {
  const local = orderDraft(draft, identities);
  const server = orderDraft(snapshot.seats.map(draftFromSnapshot), identities);
  return (
    local.length !== server.length ||
    local.some((seat, index) => !seatsAreSemanticallyEqual(seat, server[index]))
  );
}

/** Build bridge status fields from one authoritative roster snapshot. */
function bridgeStatusFromSnapshot(
  snapshot: AgentRosterSnapshot,
  paused = false,
): RoomBridgeStatus {
  return {
    paused,
    desiredRevision: snapshot.desiredRevision,
    activeRevision: snapshot.activeRevision,
    lastGoodRevision: snapshot.lastGoodRevision,
    runtimeActivation: snapshot.runtimeActivation,
    runtimeErrorCode: snapshot.runtimeErrorCode,
    runtimeErrorMessage: snapshot.runtimeErrorMessage,
  };
}

/** Initialize ordered immutable dashboard state from native server data. */
export function createAgentControlState(
  initialization: AgentControlInitialization,
): AgentControlState {
  const identities = initialization.identities.map((identity) => ({ ...identity }));
  const serverSnapshot = cloneSnapshot(initialization.snapshot);
  const draft = orderDraft(serverSnapshot.seats.map(draftFromSnapshot), identities);
  return {
    currentHumanId: initialization.currentHumanId,
    canManageRoom: initialization.canManageRoom,
    identities,
    catalog: cloneCatalog(initialization.catalog),
    serverSnapshot,
    bridgeStatus: bridgeStatusFromSnapshot(serverSnapshot),
    draft,
    dirty: false,
    revisionConflict: null,
  };
}

/** Return the current authoritative form of one stable seat. */
function baselineSeat(state: AgentControlState, seatId: string): AgentSeatSnapshot | undefined {
  return state.serverSnapshot.seats.find((seat) => seat.seatId === seatId);
}

/** Resolve the current owner for an existing or newly added seat. */
function ownerForDraftSeat(
  state: AgentControlState,
  seat: AgentSeatDraft,
): string | null {
  const baseline = baselineSeat(state, seat.seatId);
  if (baseline) {
    return baseline.ownerHumanId;
  }
  return state.identities.find((identity) => identity.id === seat.agentIdentityId)?.ownerUserId ?? null;
}

/** Test whether the signed-in human owns one seat's persistent identity. */
function mayConfigureSeat(state: AgentControlState, seat: AgentSeatDraft): boolean {
  return ownerForDraftSeat(state, seat) === state.currentHumanId;
}

/** Replace one draft seat and recompute ordered semantic dirty state. */
function updateDraftSeat(
  state: AgentControlState,
  seatId: string,
  update: (seat: AgentSeatDraft) => AgentSeatDraft,
): AgentControlState {
  const index = state.draft.findIndex((seat) => seat.seatId === seatId);
  if (index < 0) {
    return state;
  }
  const nextDraft = state.draft.map((seat, candidateIndex) =>
    candidateIndex === index ? update(cloneDraftSeat(seat)) : cloneDraftSeat(seat),
  );
  const ordered = orderDraft(nextDraft, state.identities);
  if (
    state.draft.length === ordered.length &&
    state.draft.every((seat, candidateIndex) =>
      seatsAreSemanticallyEqual(seat, ordered[candidateIndex]),
    )
  ) {
    return state;
  }
  return {
    ...state,
    draft: ordered,
    dirty: draftIsDirty(ordered, state.serverSnapshot, state.identities),
  };
}

/** Locate one available harness in the current catalog. */
function availableHarness(
  catalog: AgentCapabilityCatalog,
  harnessKey: string,
): HarnessCapability | undefined {
  return catalog.harnesses.find(
    (harness) => harness.id === harnessKey && harness.available,
  );
}

/** Test a primitive setting value against one catalog control. */
function settingValueIsValid(
  value: unknown,
  control: SettingCapabilityControl,
): boolean {
  switch (control.type) {
    case "select":
      return typeof value === "string" && control.options.some((option) => option.id === value);
    case "integer":
      return (
        typeof value === "number" &&
        Number.isInteger(value) &&
        value >= control.minimum &&
        value <= control.maximum &&
        control.step > 0 &&
        (value - control.minimum) % control.step === 0
      );
    case "boolean":
      return typeof value === "boolean";
  }
}

/** Apply an owner-controlled harness replacement with dependent invalidation. */
function setHarness(
  state: AgentControlState,
  seatId: string,
  harnessKey: string,
): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  const harness = availableHarness(state.catalog, harnessKey);
  if (!seat || !harness || !mayConfigureSeat(state, seat) || seat.harnessKey === harnessKey) {
    return state;
  }
  const modelRemainsValid = harness.models.some(
    (model) => model.id === seat.modelKey && model.available,
  );
  return updateDraftSeat(state, seatId, (current) => ({
    ...current,
    harnessKey,
    modelKey: modelRemainsValid ? current.modelKey : "",
    settings: {},
    credentialBindingId: null,
  }));
}

/** Apply one owner-controlled model selection under its current harness. */
function setModel(
  state: AgentControlState,
  seatId: string,
  modelKey: string,
): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  const harness = seat ? availableHarness(state.catalog, seat.harnessKey) : undefined;
  const model = harness?.models.find(
    (candidate) => candidate.id === modelKey && candidate.available,
  );
  if (!seat || !model || !mayConfigureSeat(state, seat) || seat.modelKey === modelKey) {
    return state;
  }
  return updateDraftSeat(state, seatId, (current) => ({ ...current, modelKey }));
}

/** Apply one owner-controlled catalog setting value. */
function setSetting(
  state: AgentControlState,
  seatId: string,
  settingId: string,
  value: AgentSettingValue,
): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  const harness = seat ? availableHarness(state.catalog, seat.harnessKey) : undefined;
  const setting = harness?.settings.find((candidate) => candidate.id === settingId);
  if (!seat || !setting || !mayConfigureSeat(state, seat) || !settingValueIsValid(value, setting.control)) {
    return state;
  }
  return updateDraftSeat(state, seatId, (current) => ({
    ...current,
    settings: { ...current.settings, [settingId]: value },
  }));
}

/** Remove one owner-controlled catalog setting value. */
function removeSetting(
  state: AgentControlState,
  seatId: string,
  settingId: string,
): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  if (!seat || !mayConfigureSeat(state, seat) || !(settingId in seat.settings)) {
    return state;
  }
  return updateDraftSeat(state, seatId, (current) => {
    const settings = { ...current.settings };
    delete settings[settingId];
    return { ...current, settings };
  });
}

/** Apply one owner-controlled opaque credential binding selection. */
function setCredentialBinding(
  state: AgentControlState,
  seatId: string,
  credentialBindingId: string | null,
): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  if (
    !seat ||
    !mayConfigureSeat(state, seat) ||
    seat.credentialBindingId === credentialBindingId
  ) {
    return state;
  }
  return updateDraftSeat(state, seatId, (current) => ({
    ...current,
    credentialBindingId,
  }));
}

/** Apply owner enablement or manager-safe disablement for one seat. */
function setEnabled(
  state: AgentControlState,
  seatId: string,
  enabled: boolean,
): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  if (!seat || seat.enabled === enabled) {
    return state;
  }
  const baseline = baselineSeat(state, seatId);
  const restoresBaseline = baseline?.enabled === enabled;
  const managerDisable = state.canManageRoom && baseline?.enabled === true && !enabled;
  if (!mayConfigureSeat(state, seat) && !restoresBaseline && !managerDisable) {
    return state;
  }
  return updateDraftSeat(state, seatId, (current) => ({ ...current, enabled }));
}

/** Add one signed-in-human-owned persistent identity at the end of the room roster. */
function addSeat(
  state: AgentControlState,
  seatId: string,
  agentIdentityId: string,
): AgentControlState {
  const identity = state.identities.find((candidate) => candidate.id === agentIdentityId);
  const duplicate = state.draft.some(
    (seat) => seat.seatId === seatId || seat.agentIdentityId === agentIdentityId,
  );
  if (!identity || identity.ownerUserId !== state.currentHumanId || duplicate || !seatId) {
    return state;
  }
  const nextPosition =
    state.draft.reduce((maximum, seat) => Math.max(maximum, seat.position), -1) + 1;
  const draft = orderDraft(
    [
      ...state.draft,
      {
        seatId,
        agentIdentityId,
        harnessKey: "",
        modelKey: "",
        settings: {},
        credentialBindingId: null,
        enabled: true,
        position: nextPosition,
      },
    ],
    state.identities,
  );
  return {
    ...state,
    draft,
    dirty: draftIsDirty(draft, state.serverSnapshot, state.identities),
  };
}

/** Remove one owned or manager-controlled room seat without deleting its identity. */
function removeSeat(state: AgentControlState, seatId: string): AgentControlState {
  const seat = state.draft.find((candidate) => candidate.seatId === seatId);
  if (!seat || (!mayConfigureSeat(state, seat) && !state.canManageRoom)) {
    return state;
  }
  const draft = orderDraft(
    state.draft.filter((candidate) => candidate.seatId !== seatId),
    state.identities,
  );
  return {
    ...state,
    draft,
    dirty: draftIsDirty(draft, state.serverSnapshot, state.identities),
  };
}

/** Move one seat under room-manager authority and normalize all positions. */
function moveSeat(
  state: AgentControlState,
  seatId: string,
  toIndex: number,
): AgentControlState {
  if (!state.canManageRoom || !Number.isInteger(toIndex)) {
    return state;
  }
  const ordered = orderDraft(state.draft, state.identities);
  const fromIndex = ordered.findIndex((seat) => seat.seatId === seatId);
  if (fromIndex < 0) {
    return state;
  }
  const boundedIndex = Math.max(0, Math.min(toIndex, ordered.length - 1));
  if (fromIndex === boundedIndex) {
    return state;
  }
  const [moved] = ordered.splice(fromIndex, 1);
  ordered.splice(boundedIndex, 0, moved);
  const draft = ordered.map((seat, position) => ({ ...seat, position }));
  return {
    ...state,
    draft,
    dirty: draftIsDirty(draft, state.serverSnapshot, state.identities),
  };
}

/** Restore the ordered local draft from current authoritative server truth. */
function discardDraft(state: AgentControlState): AgentControlState {
  const draft = orderDraft(
    state.serverSnapshot.seats.map(draftFromSnapshot),
    state.identities,
  );
  return {
    ...state,
    draft,
    dirty: false,
    revisionConflict: null,
  };
}

/** Adopt a complete authoritative roster and reset local editing state. */
function applySucceeded(
  state: AgentControlState,
  snapshot: AgentRosterSnapshot,
): AgentControlState {
  if (snapshot.serverId !== state.serverSnapshot.serverId) {
    return state;
  }
  const serverSnapshot = cloneSnapshot(snapshot);
  return {
    ...state,
    serverSnapshot,
    bridgeStatus: bridgeStatusFromSnapshot(serverSnapshot, state.bridgeStatus.paused),
    draft: orderDraft(serverSnapshot.seats.map(draftFromSnapshot), state.identities),
    dirty: false,
    revisionConflict: null,
  };
}

/** Preserve local edits while recording fresher server truth after a revision mismatch. */
function retainRevisionConflict(
  state: AgentControlState,
  snapshot: AgentRosterSnapshot,
): AgentControlState {
  if (snapshot.serverId !== state.serverSnapshot.serverId) {
    return state;
  }
  const attemptedRevision = state.serverSnapshot.desiredRevision;
  const serverSnapshot = cloneSnapshot(snapshot);
  const dirty = draftIsDirty(state.draft, serverSnapshot, state.identities);
  return {
    ...state,
    serverSnapshot,
    bridgeStatus: bridgeStatusFromSnapshot(serverSnapshot, state.bridgeStatus.paused),
    dirty,
    revisionConflict: dirty
      ? { attemptedRevision, serverRevision: serverSnapshot.desiredRevision }
      : null,
  };
}

/** Project bridge lifecycle fields without replacing the roster draft. */
function updateRuntime(
  state: AgentControlState,
  status: RoomBridgeStatus,
): AgentControlState {
  const serverSnapshot: AgentRosterSnapshot = {
    ...state.serverSnapshot,
    desiredRevision: status.desiredRevision,
    activeRevision: status.activeRevision,
    lastGoodRevision: status.lastGoodRevision,
    runtimeActivation: status.runtimeActivation,
    runtimeErrorCode: status.runtimeErrorCode,
    runtimeErrorMessage: status.runtimeErrorMessage,
    seats: state.serverSnapshot.seats.map((seat) => ({
      ...seat,
      settings: cloneSettings(seat.settings),
      runtimeActivation: status.runtimeActivation,
    })),
  };
  return {
    ...state,
    serverSnapshot,
    bridgeStatus: { ...status },
    dirty: draftIsDirty(state.draft, serverSnapshot, state.identities),
  };
}

/** Apply one pure room agent configuration transition. */
export function applyAgentControlAction(
  state: AgentControlState,
  action: AgentControlAction,
): AgentControlState {
  switch (action.type) {
    case "setHarness":
      return setHarness(state, action.seatId, action.harnessKey);
    case "setModel":
      return setModel(state, action.seatId, action.modelKey);
    case "setSetting":
      return setSetting(state, action.seatId, action.settingId, action.value);
    case "removeSetting":
      return removeSetting(state, action.seatId, action.settingId);
    case "setCredentialBinding":
      return setCredentialBinding(state, action.seatId, action.credentialBindingId);
    case "setEnabled":
      return setEnabled(state, action.seatId, action.enabled);
    case "addSeat":
      return addSeat(state, action.seatId, action.agentIdentityId);
    case "removeSeat":
      return removeSeat(state, action.seatId);
    case "moveSeat":
      return moveSeat(state, action.seatId, action.toIndex);
    case "discard":
      return discardDraft(state);
    case "applySucceeded":
      return applySucceeded(state, action.snapshot);
    case "revisionConflict":
      return retainRevisionConflict(state, action.snapshot);
    case "runtimeUpdated":
      return updateRuntime(state, action.status);
    case "catalogUpdated":
      return { ...state, catalog: cloneCatalog(action.catalog) };
  }
}

/** Construct one stable safe validation issue. */
function validationIssue(
  code: AgentControlValidationCode,
  message: string,
  seatId: string | null = null,
  field: string | null = null,
): AgentControlValidationIssue {
  return { code, seatId, field, message };
}

/** Measure one settings object exactly as UTF-8 JSON submitted to native code. */
function settingsByteLength(settings: Readonly<Record<string, unknown>>): number {
  try {
    return new TextEncoder().encode(JSON.stringify(settings)).byteLength;
  } catch {
    return Number.POSITIVE_INFINITY;
  }
}

/** Validate one seat against the current deployment capability catalog. */
function validateSeatCatalog(
  state: AgentControlState,
  seat: AgentSeatDraft,
): AgentControlValidationIssue[] {
  const issues: AgentControlValidationIssue[] = [];
  if (!seat.harnessKey) {
    issues.push(
      validationIssue(
        "harness_required",
        "Choose an execution harness.",
        seat.seatId,
        "harnessKey",
      ),
    );
    return issues;
  }
  const harness = state.catalog.harnesses.find((candidate) => candidate.id === seat.harnessKey);
  if (!harness || !harness.available) {
    issues.push(
      validationIssue(
        "harness_unavailable",
        "The selected execution harness is not available on this deployment.",
        seat.seatId,
        "harnessKey",
      ),
    );
    return issues;
  }
  if (!seat.modelKey) {
    issues.push(
      validationIssue("model_required", "Choose a model.", seat.seatId, "modelKey"),
    );
  } else {
    const model = harness.models.find((candidate) => candidate.id === seat.modelKey);
    if (!model || !model.available) {
      issues.push(
        validationIssue(
          "model_unavailable",
          "The selected model is not available for this harness.",
          seat.seatId,
          "modelKey",
        ),
      );
    }
  }
  const settingsById = new Map(harness.settings.map((setting) => [setting.id, setting]));
  for (const [settingId, value] of Object.entries(seat.settings)) {
    const setting = settingsById.get(settingId);
    if (!setting) {
      issues.push(
        validationIssue(
          "unknown_setting",
          "Remove the setting that is no longer declared by this harness.",
          seat.seatId,
          settingId,
        ),
      );
    } else if (!settingValueIsValid(value, setting.control)) {
      issues.push(
        validationIssue(
          "setting_invalid",
          `Choose a valid value for ${setting.label}.`,
          seat.seatId,
          settingId,
        ),
      );
    }
  }
  for (const setting of harness.settings) {
    if (setting.required && !(setting.id in seat.settings)) {
      issues.push(
        validationIssue(
          "setting_required",
          `${setting.label} is required.`,
          seat.seatId,
          setting.id,
        ),
      );
    }
  }
  if (settingsByteLength(seat.settings) > MAX_AGENT_SETTINGS_BYTES) {
    issues.push(
      validationIssue(
        "settings_too_large",
        "Agent settings exceed the room roster limit.",
        seat.seatId,
        "settings",
      ),
    );
  }
  if (harness.credentialMode === "requiredBinding" && !seat.credentialBindingId) {
    issues.push(
      validationIssue(
        "credential_required",
        "Select a ready credential binding for this harness.",
        seat.seatId,
        "credentialBindingId",
      ),
    );
  }
  const baseline = baselineSeat(state, seat.seatId);
  if (
    seat.enabled &&
    baseline &&
    (baseline.credentialReadiness === "unavailable" ||
      baseline.credentialReadiness === "attention") &&
    baseline.credentialBindingId === seat.credentialBindingId &&
    baseline.harnessKey === seat.harnessKey
  ) {
    issues.push(
      validationIssue(
        "credential_not_ready",
        "The selected credential is not ready for use.",
        seat.seatId,
        "credentialBindingId",
      ),
    );
  }
  return issues;
}

/** Validate owner and manager authority against the immutable server snapshot. */
function validateAuthorization(state: AgentControlState): AgentControlValidationIssue[] {
  const issues: AgentControlValidationIssue[] = [];
  const baselineBySeat = new Map(
    state.serverSnapshot.seats.map((seat) => [seat.seatId, seat]),
  );
  const baselineByAgent = new Map(
    state.serverSnapshot.seats.map((seat) => [seat.agentIdentityId, seat]),
  );
  const draftByAgent = new Map(state.draft.map((seat) => [seat.agentIdentityId, seat]));
  for (const seat of state.draft) {
    const seatBaseline = baselineBySeat.get(seat.seatId);
    if (seatBaseline && seatBaseline.agentIdentityId !== seat.agentIdentityId) {
      issues.push(
        validationIssue(
          "seat_identity_changed",
          "An existing room seat must retain its persistent agent identity.",
          seat.seatId,
          "agentIdentityId",
        ),
      );
    }
    const baseline = baselineByAgent.get(seat.agentIdentityId);
    const owner = baseline?.ownerHumanId ??
      state.identities.find((identity) => identity.id === seat.agentIdentityId)?.ownerUserId ??
      null;
    const ownerIsCurrentHuman = owner === state.currentHumanId;
    if (!baseline) {
      if (!ownerIsCurrentHuman) {
        issues.push(
          validationIssue(
            "owner_required",
            "Only an identity owner may add its room seat.",
            seat.seatId,
            "agentIdentityId",
          ),
        );
      }
      continue;
    }
    if (baseline.seatId !== seat.seatId) {
      issues.push(
        validationIssue(
          "seat_identity_changed",
          "An existing agent must retain its stable room seat identifier.",
          seat.seatId,
          "seatId",
        ),
      );
    }
    const baselineDraft = draftFromSnapshot(baseline);
    const configurationChanged =
      baselineDraft.harnessKey !== seat.harnessKey ||
      baselineDraft.modelKey !== seat.modelKey ||
      canonicalSettings(baselineDraft.settings) !== canonicalSettings(seat.settings) ||
      baselineDraft.credentialBindingId !== seat.credentialBindingId;
    if (configurationChanged && !ownerIsCurrentHuman) {
      issues.push(
        validationIssue(
          "owner_required",
          "Only the identity owner may change execution configuration.",
          seat.seatId,
          "configuration",
        ),
      );
    }
    if (baseline.enabled !== seat.enabled) {
      const managerDisable = baseline.enabled && !seat.enabled && state.canManageRoom;
      if (!ownerIsCurrentHuman && !managerDisable) {
        issues.push(
          validationIssue(
            "owner_required",
            "Only the identity owner may enable this seat.",
            seat.seatId,
            "enabled",
          ),
        );
      }
    }
    if (baseline.position !== seat.position && !state.canManageRoom) {
      issues.push(
        validationIssue(
          "manager_required",
          "Room manager permission is required to reorder seats.",
          seat.seatId,
          "position",
        ),
      );
    }
  }
  for (const baseline of state.serverSnapshot.seats) {
    if (!draftByAgent.has(baseline.agentIdentityId)) {
      const mayRemove =
        baseline.ownerHumanId === state.currentHumanId || state.canManageRoom;
      if (!mayRemove) {
        issues.push(
          validationIssue(
            "owner_required",
            "Only the identity owner or a room manager may remove this seat.",
            baseline.seatId,
            "seatId",
          ),
        );
      }
    }
  }
  return issues;
}

/** Validate a complete local roster draft before it crosses the native boundary. */
export function validateAgentControlDraft(
  state: AgentControlState,
): AgentControlValidationIssue[] {
  const issues: AgentControlValidationIssue[] = [];
  if (state.draft.length > MAX_AGENT_SEATS) {
    issues.push(
      validationIssue(
        "too_many_seats",
        `A room may contain at most ${MAX_AGENT_SEATS} agent seats.`,
      ),
    );
  }
  const seatIds = new Set<string>();
  const agentIds = new Set<string>();
  const positions = new Set<number>();
  for (const seat of state.draft) {
    if (seatIds.has(seat.seatId)) {
      issues.push(
        validationIssue("duplicate_seat", "Room seat identifiers must be unique.", seat.seatId),
      );
    }
    seatIds.add(seat.seatId);
    if (agentIds.has(seat.agentIdentityId)) {
      issues.push(
        validationIssue(
          "duplicate_agent",
          "An agent identity may occupy only one room seat.",
          seat.seatId,
          "agentIdentityId",
        ),
      );
    }
    agentIds.add(seat.agentIdentityId);
    if (!Number.isInteger(seat.position) || seat.position < 0) {
      issues.push(
        validationIssue(
          "invalid_position",
          "Room seat positions must be non-negative integers.",
          seat.seatId,
          "position",
        ),
      );
    } else if (positions.has(seat.position)) {
      issues.push(
        validationIssue(
          "duplicate_position",
          "Room seat positions must be unique.",
          seat.seatId,
          "position",
        ),
      );
    }
    positions.add(seat.position);
    issues.push(...validateSeatCatalog(state, seat));
  }
  issues.push(...validateAuthorization(state));
  return issues;
}

/** Serialize one valid draft as a complete revision-checked roster replacement. */
export function serializeAgentControlDraft(
  state: AgentControlState,
): AgentControlSerializationResult {
  const issues = validateAgentControlDraft(state);
  if (issues.length > 0) {
    return { ok: false, issues };
  }
  return {
    ok: true,
    request: {
      expectedRevision: state.serverSnapshot.desiredRevision,
      seats: orderDraft(state.draft, state.identities),
    },
  };
}
