/** Accessible owner-aware controls for one persistent room agent seat. */
import { useId } from "react";
import type {
  AgentControlAction,
  AgentControlState,
  AgentControlValidationIssue,
  AgentIdentity,
  AgentSeatDraft,
  AgentSeatSnapshot,
  HarnessCapability,
  SettingCapability,
} from "../domain/agentControl";
import { CapabilitySelect } from "./CapabilitySelect";

/** Inputs required to render and edit one seat. */
export interface AgentSeatCardProps {
  /** Complete reducer state used for ownership and catalog decisions. */
  readonly control: AgentControlState;
  /** Current immutable seat draft. */
  readonly seat: AgentSeatDraft;
  /** Send one intent through the authoritative room-agent reducer. */
  readonly onAction: (action: AgentControlAction) => void;
  /** Local validation failures attached to this stable seat. */
  readonly validationIssues?: readonly AgentControlValidationIssue[];
  /** Whether every roster edit is frozen during one complete mutation. */
  readonly editingDisabled?: boolean;
}

/** Inputs for one catalog-defined setting control. */
interface AgentSettingControlProps {
  /** Stable DOM identifier prefix. */
  readonly id: string;
  /** Human-facing agent name included in accessible labels. */
  readonly agentName: string;
  /** Current immutable seat draft. */
  readonly seat: AgentSeatDraft;
  /** Catalog metadata defining the only accepted control values. */
  readonly setting: SettingCapability;
  /** Whether the signed-in human may configure this identity. */
  readonly disabled: boolean;
  /** Send one setting change through the authoritative reducer. */
  readonly onAction: (action: AgentControlAction) => void;
}

/** Human-readable runtime lines for one card. */
interface RuntimeSummary {
  /** Primary activation state. */
  readonly primary: string;
  /** Optional recovery context from the last proven revision. */
  readonly secondary: string | null;
}

/** Resolve one seat's persistent identity from visible dashboard context. */
function findIdentity(
  state: AgentControlState,
  seat: AgentSeatDraft,
): AgentIdentity | undefined {
  return state.identities.find((identity) => identity.id === seat.agentIdentityId);
}

/** Resolve the immutable server baseline for permission and readiness context. */
function findBaseline(
  state: AgentControlState,
  seatId: string,
): AgentSeatSnapshot | undefined {
  return state.serverSnapshot.seats.find((seat) => seat.seatId === seatId);
}

/** Resolve the current owner without transferring ownership in the browser. */
function ownerId(
  identity: AgentIdentity | undefined,
  baseline: AgentSeatSnapshot | undefined,
): string | null {
  return baseline ? baseline.ownerHumanId : identity?.ownerUserId ?? null;
}

/** Describe identity ownership without inventing unavailable human names. */
function ownerLabel(owner: string | null, currentHumanId: string): string {
  if (owner === currentHumanId) {
    return "You own this identity";
  }
  return owner ? "Owned by another member" : "Needs an owner";
}

/** Describe opaque credential readiness without exposing host or secret metadata. */
function credentialLabel(baseline: AgentSeatSnapshot | undefined): string {
  switch (baseline?.credentialReadiness) {
    case "hostSession":
      return "Host session ready";
    case "ready":
      return "Credential ready";
    case "unavailable":
      return "Credential unavailable";
    case "attention":
      return "Credential needs attention";
    default:
      return "Checked after apply";
  }
}

/** Summarize room-wide activation truth for one desired seat. */
function runtimeSummary(
  state: AgentControlState,
  baseline: AgentSeatSnapshot | undefined,
): RuntimeSummary {
  if (!baseline) {
    return { primary: "Not yet applied", secondary: null };
  }
  const runtime = state.bridgeStatus;
  switch (runtime.runtimeActivation) {
    case "active":
      return {
        primary: `Active · revision ${runtime.activeRevision ?? baseline.configurationRevision ?? "unknown"}`,
        secondary: null,
      };
    case "pending":
      return {
        primary: `Pending · desired revision ${runtime.desiredRevision ?? "unknown"}`,
        secondary:
          runtime.lastGoodRevision === null
            ? null
            : `Last good revision ${runtime.lastGoodRevision}`,
      };
    case "failed":
      return {
        primary: `Failed · ${runtime.runtimeErrorCode ?? "activation error"}`,
        secondary:
          runtime.lastGoodRevision === null
            ? "Review configuration before retrying."
            : `Last good revision ${runtime.lastGoodRevision}`,
      };
    case "idle":
      return { primary: "Idle", secondary: null };
  }
}

/** Render one typed catalog setting and preserve absent optional values. */
function AgentSettingControl({
  id,
  agentName,
  seat,
  setting,
  disabled,
  onAction,
}: AgentSettingControlProps) {
  const value = seat.settings[setting.id];
  const label = `${setting.label}${setting.required ? " · required" : ""}`;
  const ariaLabel = `${setting.label} for ${agentName}`;

  if (setting.control.type === "select") {
    return (
      <CapabilitySelect
        id={id}
        label={label}
        ariaLabel={ariaLabel}
        value={typeof value === "string" ? value : ""}
        options={setting.control.options.map((option) => ({
          ...option,
          available: true,
        }))}
        placeholder={`Choose ${setting.label.toLowerCase()}`}
        disabled={disabled}
        onChange={(nextValue) =>
          onAction({
            type: "setSetting",
            seatId: seat.seatId,
            settingId: setting.id,
            value: nextValue,
          })
        }
      />
    );
  }

  if (setting.control.type === "integer") {
    return (
      <label className="agent-field" htmlFor={id}>
        <span>{label}</span>
        <input
          id={id}
          aria-label={ariaLabel}
          type="number"
          min={setting.control.minimum}
          max={setting.control.maximum}
          step={setting.control.step}
          value={typeof value === "number" ? value : ""}
          disabled={disabled}
          onChange={(event) => {
            const nextValue = event.currentTarget.value;
            onAction(
              nextValue === ""
                ? { type: "removeSetting", seatId: seat.seatId, settingId: setting.id }
                : {
                    type: "setSetting",
                    seatId: seat.seatId,
                    settingId: setting.id,
                    value: Number(nextValue),
                  },
            );
          }}
        />
      </label>
    );
  }

  return (
    <label className="agent-toggle agent-toggle--setting" htmlFor={id}>
      <span>{label}</span>
      <input
        id={id}
        aria-label={ariaLabel}
        type="checkbox"
        role="switch"
        checked={value === true}
        disabled={disabled}
        onChange={(event) =>
          onAction({
            type: "setSetting",
            seatId: seat.seatId,
            settingId: setting.id,
            value: event.currentTarget.checked,
          })
        }
      />
    </label>
  );
}

/** Render one room seat with controls bounded by identity ownership and manager policy. */
export function AgentSeatCard({
  control,
  seat,
  onAction,
  validationIssues = [],
  editingDisabled = false,
}: AgentSeatCardProps) {
  const idPrefix = useId();
  const identity = findIdentity(control, seat);
  const baseline = findBaseline(control, seat.seatId);
  const owner = ownerId(identity, baseline);
  const agentName = identity?.displayName?.trim() || identity?.username || seat.agentIdentityId;
  const username = identity?.username || baseline?.agentUsername || seat.agentIdentityId;
  const mayConfigure = !editingDisabled && owner === control.currentHumanId;
  const mayRemove = !editingDisabled && (mayConfigure || control.canManageRoom);
  const toggleRestoresBaseline =
    baseline !== undefined && baseline.enabled !== seat.enabled;
  const managerMayDisable =
    control.canManageRoom && baseline?.enabled === true && seat.enabled;
  const mayToggle =
    !editingDisabled &&
    (mayConfigure || toggleRestoresBaseline || managerMayDisable);
  const harness = control.catalog.harnesses.find(
    (candidate) => candidate.id === seat.harnessKey,
  );
  const harnessUsable = harness?.available === true;
  const runtime = runtimeSummary(control, baseline);

  return (
    <article className="agent-seat-card" aria-labelledby={`${idPrefix}-title`}>
      <header className="agent-seat-card__header">
        <div>
          <p className="agent-seat-card__index">Seat {seat.position + 1}</p>
          <h4 id={`${idPrefix}-title`}>{agentName}</h4>
          <p className="agent-seat-card__identity">@{username}</p>
        </div>
        <span className={`agent-owner agent-owner--${owner ? (mayConfigure ? "self" : "other") : "unowned"}`}>
          {ownerLabel(owner, control.currentHumanId)}
        </span>
      </header>

      <div className="agent-seat-card__signals" aria-label={`Status for ${agentName}`}>
        <div>
          <span>Credential</span>
          <strong>{credentialLabel(baseline)}</strong>
        </div>
        <div>
          <span>Runtime</span>
          <strong>{runtime.primary}</strong>
          {runtime.secondary ? <small>{runtime.secondary}</small> : null}
        </div>
      </div>

      {validationIssues.length > 0 ? (
        <div className="agent-seat-card__validation" role="alert">
          <strong>Configuration needs attention</strong>
          <ul>
            {validationIssues.map((issue) => (
              <li key={`${issue.code}-${issue.field ?? "seat"}`}>{issue.message}</li>
            ))}
          </ul>
        </div>
      ) : null}

      <div className="agent-seat-card__controls">
        <CapabilitySelect
          id={`${idPrefix}-harness`}
          label="Execution harness"
          ariaLabel={`Execution harness for ${agentName}`}
          value={seat.harnessKey}
          options={control.catalog.harnesses}
          placeholder="Choose a harness"
          disabled={!mayConfigure}
          onChange={(harnessKey) =>
            onAction({ type: "setHarness", seatId: seat.seatId, harnessKey })
          }
        />
        <CapabilitySelect
          id={`${idPrefix}-model`}
          label="Model"
          ariaLabel={`Model for ${agentName}`}
          value={seat.modelKey}
          options={harness?.models ?? []}
          placeholder={seat.harnessKey ? "Choose a model" : "Choose a harness first"}
          disabled={!mayConfigure || !harnessUsable}
          onChange={(modelKey) =>
            onAction({ type: "setModel", seatId: seat.seatId, modelKey })
          }
        />
        {harness?.settings.map((setting) => (
          <AgentSettingControl
            key={setting.id}
            id={`${idPrefix}-${setting.id}`}
            agentName={agentName}
            seat={seat}
            setting={setting}
            disabled={!mayConfigure || !harnessUsable}
            onAction={onAction}
          />
        ))}
      </div>

      <footer className="agent-seat-card__footer">
        <label className="agent-toggle" htmlFor={`${idPrefix}-enabled`}>
          <span>{seat.enabled ? "Participating" : "Disabled"}</span>
          <input
            id={`${idPrefix}-enabled`}
            aria-label={`Enable ${agentName} in this room`}
            type="checkbox"
            role="switch"
            checked={seat.enabled}
            disabled={!mayToggle}
            onChange={(event) =>
              onAction({
                type: "setEnabled",
                seatId: seat.seatId,
                enabled: event.currentTarget.checked,
              })
            }
          />
        </label>
        <button
          className="agent-remove-button"
          type="button"
          disabled={!mayRemove}
          aria-label={`Remove ${agentName} from room`}
          onClick={() => onAction({ type: "removeSeat", seatId: seat.seatId })}
        >
          Remove from room
        </button>
      </footer>
    </article>
  );
}
