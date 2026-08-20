/** Persistent roster-save, conflict, and activation feedback for the Agents tab. */
import { useState } from "react";
import type {
  AgentControlState,
  AgentControlValidationIssue,
  AgentRevisionConflictSeat,
} from "../domain/agentControl";

/** Inputs for one room's atomic roster workflow controls. */
export interface DashboardApplyBarProps {
  /** Current reducer state supplying dirty, conflict, and runtime truth. */
  readonly control: AgentControlState;
  /** Latest local validation failures from an attempted apply. */
  readonly validationIssues: readonly AgentControlValidationIssue[];
  /** Latest sanitized transport failure, absent before or after recovery. */
  readonly errorMessage: string | null;
  /** Whether one complete roster replacement is currently in flight. */
  readonly saving: boolean;
  /** Whether bounded activation polling reached its time limit. */
  readonly activationTimedOut: boolean;
  /** Submit the complete current roster through the client boundary. */
  readonly onApply: () => void;
  /** Restore the latest authoritative roster and clear local workflow errors. */
  readonly onDiscard: () => void;
  /** Reconcile the current desired revision without creating another revision. */
  readonly onRetryActivation: () => void;
  /** Read bridge status once after bounded polling stops. */
  readonly onRefreshActivation: () => void;
}

/** Resolve a stable human-facing identity label for one conflict seat. */
function conflictSeatLabel(
  control: AgentControlState,
  seat: AgentRevisionConflictSeat,
): string {
  const identity = control.identities.find(
    (candidate) => candidate.id === seat.agentIdentityId,
  );
  return identity?.displayName?.trim() || identity?.username || seat.agentIdentityId;
}

/** Format a changed-field collection without exposing draft values. */
function changedFieldsLabel(fields: readonly string[]): string {
  return fields.length > 0 ? fields.join(", ") : "No changes";
}

/** Render atomic Apply and Discard controls with explicit recovery paths. */
export function DashboardApplyBar({
  control,
  validationIssues,
  errorMessage,
  saving,
  activationTimedOut,
  onApply,
  onDiscard,
  onRetryActivation,
  onRefreshActivation,
}: DashboardApplyBarProps) {
  const [reviewingConflict, setReviewingConflict] = useState(false);
  const runtime = control.bridgeStatus;
  const conflict = control.revisionConflict;
  const rosterIssues = validationIssues.filter((issue) => issue.seatId === null);
  const seatIssueCount = validationIssues.length - rosterIssues.length;
  const visible =
    control.dirty ||
    conflict !== null ||
    errorMessage !== null ||
    runtime.runtimeActivation === "pending" ||
    runtime.runtimeActivation === "failed";

  if (!visible) {
    return null;
  }

  return (
    <aside className="dashboard-apply-bar" aria-label="Agent roster changes">
      {conflict ? (
        <div className="dashboard-apply-bar__notice" role="alert">
          <strong>Room changes arrived before your draft was applied.</strong>
          <p>
            {`Your draft started at revision ${conflict.attemptedRevision ?? "none"}. The server is now at revision ${conflict.serverRevision ?? "none"}.`}
          </p>
          {reviewingConflict ? (
            <ol className="dashboard-apply-bar__conflicts">
              {conflict.seats.map((seat) => (
                <li key={seat.seatId}>
                  <strong>{conflictSeatLabel(control, seat)}</strong>
                  <span>{`Mine: ${changedFieldsLabel(seat.localFields)}`}</span>
                  <span>{`Server: ${changedFieldsLabel(seat.serverFields)}`}</span>
                </li>
              ))}
            </ol>
          ) : null}
          <div className="dashboard-apply-bar__actions">
            <button
              className="button button-secondary"
              type="button"
              onClick={() => setReviewingConflict((reviewing) => !reviewing)}
            >
              {reviewingConflict ? "Hide changes" : "Review changes"}
            </button>
            <button
              className="button button-primary"
              type="button"
              onClick={onDiscard}
            >
              Discard mine
            </button>
          </div>
        </div>
      ) : null}

      {validationIssues.length > 0 && !conflict ? (
        <div className="dashboard-apply-bar__notice" role="alert">
          <strong>Fix the highlighted roster configuration.</strong>
          {rosterIssues.map((issue) => (
            <p key={`${issue.code}-${issue.field ?? "roster"}`}>{issue.message}</p>
          ))}
          {seatIssueCount > 0 ? (
            <p>{`${seatIssueCount} ${seatIssueCount === 1 ? "issue is" : "issues are"} attached to the affected agent seats.`}</p>
          ) : null}
        </div>
      ) : null}

      {errorMessage ? (
        <div className="dashboard-apply-bar__notice" role="alert">
          <strong>Roster changes were not saved.</strong>
          <p>{errorMessage}</p>
        </div>
      ) : null}

      {runtime.runtimeActivation === "pending" ? (
        <div className="dashboard-apply-bar__runtime" role="status">
          <div>
            <strong>{`Activating revision ${runtime.desiredRevision ?? "unknown"}`}</strong>
            <p>
              {activationTimedOut
                ? "Activation is taking longer than expected. The desired roster remains pending."
                : "Henosis is checking the room bridge without replacing the last good revision."}
            </p>
          </div>
          {activationTimedOut ? (
            <button
              className="button button-secondary"
              type="button"
              disabled={saving}
              onClick={onRefreshActivation}
            >
              Refresh status
            </button>
          ) : (
            <span className="dashboard-apply-bar__pulse" aria-hidden="true" />
          )}
        </div>
      ) : null}

      {runtime.runtimeActivation === "failed" ? (
        <div className="dashboard-apply-bar__runtime" role="alert">
          <div>
            <strong>Activation failed</strong>
            <p>
              {runtime.runtimeErrorMessage ??
                "The desired roster could not activate. The last good revision remains available."}
            </p>
            {runtime.lastGoodRevision !== null ? (
              <small>{`Last good revision ${runtime.lastGoodRevision}`}</small>
            ) : null}
          </div>
          <button
            className="button button-secondary"
            type="button"
            disabled={saving}
            onClick={onRetryActivation}
          >
            Retry activation
          </button>
        </div>
      ) : null}

      {control.dirty && !conflict ? (
        <div className="dashboard-apply-bar__actions">
          <button
            className="button button-primary"
            type="button"
            disabled={saving}
            onClick={onApply}
          >
            {saving ? "Applying..." : "Apply roster"}
          </button>
          <button
            className="button button-secondary"
            type="button"
            disabled={saving}
            onClick={onDiscard}
          >
            Discard changes
          </button>
        </div>
      ) : null}
    </aside>
  );
}
