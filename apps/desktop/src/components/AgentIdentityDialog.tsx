/** Accessible owned-agent selection, creation, and imported-identity claim dialog. */
import { useEffect, useId, useRef, useState } from "react";
import type {
  FormEvent,
  KeyboardEvent,
  MouseEvent,
  RefObject,
} from "react";
import type {
  OwnedAgentIdentity,
  UnownedAgentIdentity,
} from "../domain/agentControl";
import { normalizeClientError } from "../services/henosisClient";

/** One identity mutation that succeeded before its context refresh completed. */
interface PendingIdentityRefresh {
  /** Server-returned owned identity that must be projected into dashboard state. */
  readonly identity: OwnedAgentIdentity;
  /** Whether the new identity should also become a local room seat draft. */
  readonly addToRoom: boolean;
}

/** Field-level validation messages for public identity creation fields. */
interface IdentityFieldErrors {
  /** UTF-8 byte-length failure for the unique login handle. */
  readonly username?: string;
  /** Unicode character-count failure for the optional display name. */
  readonly displayName?: string;
}

/** Inputs for selecting, creating, or claiming one persistent agent identity. */
export interface AgentIdentityDialogProps {
  /** Current-human-owned identities absent from the selected room. */
  readonly ownedIdentities: readonly OwnedAgentIdentity[];
  /** Unowned identities visible through the selected room roster. */
  readonly unownedIdentities: readonly UnownedAgentIdentity[];
  /** Whether the current human has room-manager claim authority. */
  readonly canClaim: boolean;
  /** Add one already-owned identity to the local room draft. */
  readonly onSelectIdentity: (identityId: string) => void;
  /** Create one persistent identity through the authenticated client. */
  readonly onCreateIdentity: (
    username: string,
    displayName: string | null,
  ) => Promise<OwnedAgentIdentity>;
  /** Claim one roster-visible imported identity through the authenticated client. */
  readonly onClaimIdentity: (
    agentIdentityId: string,
  ) => Promise<OwnedAgentIdentity>;
  /** Refresh identity and roster truth after one successful server mutation. */
  readonly onIdentityMutated: (
    identity: OwnedAgentIdentity,
    addToRoom: boolean,
  ) => Promise<void>;
  /** Close only this nested dialog. */
  readonly onClose: () => void;
  /** Trigger that regains focus when the nested dialog closes. */
  readonly returnFocusRef: RefObject<HTMLButtonElement | null>;
}

/** Selector for controls that may participate in modal keyboard containment. */
const FOCUSABLE_SELECTOR = [
  "button:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "textarea:not([disabled])",
  "[href]",
  "[tabindex]:not([tabindex='-1'])",
].join(",");

/** Resolve the public name used in labels and confirmation copy. */
function identityName(identity: OwnedAgentIdentity | UnownedAgentIdentity): string {
  return identity.displayName?.trim() || identity.username;
}

/** Validate exactly the public limits enforced by the current Rift server. */
function validateIdentityFields(
  username: string,
  displayName: string,
): IdentityFieldErrors {
  const errors: { username?: string; displayName?: string } = {};
  const usernameBytes = new TextEncoder().encode(username.trim()).byteLength;
  if (usernameBytes < 3 || usernameBytes > 32) {
    errors.username = "Handle must be between 3 and 32 UTF-8 bytes.";
  }
  if (Array.from(displayName.trim()).length > 64) {
    errors.displayName = "Display name must be 64 characters or fewer.";
  }
  return errors;
}

/** Render an accessible nested dialog with recoverable identity mutations. */
export function AgentIdentityDialog({
  ownedIdentities,
  unownedIdentities,
  canClaim,
  onSelectIdentity,
  onCreateIdentity,
  onClaimIdentity,
  onIdentityMutated,
  onClose,
  returnFocusRef,
}: AgentIdentityDialogProps) {
  const titleId = useId();
  const dialogRef = useRef<HTMLDivElement>(null);
  const closeButtonRef = useRef<HTMLButtonElement>(null);
  const [username, setUsername] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [fieldErrors, setFieldErrors] = useState<IdentityFieldErrors>({});
  const [actionError, setActionError] = useState<string | null>(null);
  const [claimCandidate, setClaimCandidate] = useState<UnownedAgentIdentity | null>(null);
  const [pendingRefresh, setPendingRefresh] = useState<PendingIdentityRefresh | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    closeButtonRef.current?.focus();
    return () => {
      returnFocusRef.current?.focus();
    };
  }, [returnFocusRef]);

  /** Close on Escape and contain Tab navigation inside this nested modal. */
  function handleDialogKeyDown(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      if (!busy && !pendingRefresh) {
        onClose();
      }
      return;
    }
    if (event.key !== "Tab") {
      return;
    }
    event.stopPropagation();
    const focusable = Array.from(
      dialogRef.current?.querySelectorAll<HTMLElement>(FOCUSABLE_SELECTOR) ?? [],
    ).filter((element) => !element.hasAttribute("hidden"));
    if (focusable.length === 0) {
      event.preventDefault();
      return;
    }
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  /** Close only when the pointer activates the dimmed backdrop itself. */
  function handleBackdropClick(event: MouseEvent<HTMLDivElement>): void {
    if (event.target === event.currentTarget && !busy && !pendingRefresh) {
      onClose();
    }
  }

  /** Finalize one already-successful mutation without repeating it on retry. */
  async function finalizeMutation(pending: PendingIdentityRefresh): Promise<void> {
    setBusy(true);
    setActionError(null);
    setPendingRefresh(pending);
    try {
      await onIdentityMutated(pending.identity, pending.addToRoom);
      setPendingRefresh(null);
      onClose();
    } catch (error) {
      setActionError(normalizeClientError(error).message);
    } finally {
      setBusy(false);
    }
  }

  /** Submit one validated identity creation request. */
  async function handleCreate(event: FormEvent<HTMLFormElement>): Promise<void> {
    event.preventDefault();
    if (busy || pendingRefresh) {
      return;
    }
    const errors = validateIdentityFields(username, displayName);
    setFieldErrors(errors);
    if (errors.username || errors.displayName) {
      return;
    }
    setBusy(true);
    setActionError(null);
    const normalizedUsername = username.trim();
    const normalizedDisplayName = displayName.trim() || null;
    try {
      const identity = await onCreateIdentity(
        normalizedUsername,
        normalizedDisplayName,
      );
      await finalizeMutation({ identity, addToRoom: true });
    } catch (error) {
      setActionError(normalizeClientError(error).message);
      setBusy(false);
    }
  }

  /** Submit one explicitly confirmed imported-identity claim. */
  async function confirmClaim(): Promise<void> {
    if (!claimCandidate || !canClaim || busy || pendingRefresh) {
      return;
    }
    setBusy(true);
    setActionError(null);
    try {
      const identity = await onClaimIdentity(claimCandidate.id);
      await finalizeMutation({ identity, addToRoom: false });
    } catch (error) {
      setActionError(normalizeClientError(error).message);
      setBusy(false);
    }
  }

  return (
    <div
      className="agent-identity-dialog-backdrop"
      onClick={handleBackdropClick}
      role="presentation"
    >
      <div
        ref={dialogRef}
        className="agent-identity-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        onKeyDown={handleDialogKeyDown}
      >
        <header className="agent-identity-dialog__header">
          <div>
            <p className="eyebrow">Persistent identity</p>
            <h3 id={titleId}>Add an agent identity</h3>
            <p>Select one you own, create one, or claim a visible imported identity.</p>
          </div>
          <button
            ref={closeButtonRef}
            className="icon-button"
            type="button"
            aria-label="Close agent identity dialog"
            disabled={busy || Boolean(pendingRefresh)}
            onClick={onClose}
          >
            ×
          </button>
        </header>

        {actionError ? <p role="alert" className="dashboard-error">{actionError}</p> : null}
        {pendingRefresh ? (
          <div className="agent-identity-dialog__recovery">
            <p>The identity change succeeded, but room context did not refresh.</p>
            <button
              className="button button-primary"
              type="button"
              disabled={busy}
              onClick={() => void finalizeMutation(pendingRefresh)}
            >
              {busy ? "Refreshing…" : "Retry refresh"}
            </button>
          </div>
        ) : null}

        <section className="agent-identity-dialog__section" aria-labelledby={`${titleId}-owned`}>
          <div>
            <p className="eyebrow">Already yours</p>
            <h4 id={`${titleId}-owned`}>Your identities</h4>
          </div>
          {ownedIdentities.length === 0 ? (
            <p>Every identity you own is already in this room.</p>
          ) : (
            <div className="agent-identity-dialog__identity-list">
              {ownedIdentities.map((identity) => (
                <div className="agent-identity-dialog__identity" key={identity.id}>
                  <div>
                    <strong>{identityName(identity)}</strong>
                    <span>@{identity.username}</span>
                  </div>
                  <button
                    className="button button-secondary"
                    type="button"
                    disabled={busy || Boolean(pendingRefresh)}
                    aria-label={`Add ${identityName(identity)} to room`}
                    onClick={() => {
                      onSelectIdentity(identity.id);
                      onClose();
                    }}
                  >
                    Add to room
                  </button>
                </div>
              ))}
            </div>
          )}
        </section>

        <section className="agent-identity-dialog__section" aria-labelledby={`${titleId}-create`}>
          <div>
            <p className="eyebrow">New identity</p>
            <h4 id={`${titleId}-create`}>Create an agent</h4>
          </div>
          <form className="agent-identity-dialog__form" onSubmit={(event) => void handleCreate(event)}>
            <div className="agent-identity-dialog__field">
              <label>
                <span>Handle</span>
                <input
                  type="text"
                  autoComplete="off"
                  value={username}
                  disabled={busy || Boolean(pendingRefresh)}
                  aria-invalid={fieldErrors.username ? "true" : undefined}
                  aria-describedby={fieldErrors.username ? `${titleId}-username-error` : undefined}
                  onChange={(event) => setUsername(event.currentTarget.value)}
                />
              </label>
              {fieldErrors.username ? (
                <p id={`${titleId}-username-error`} className="field-error">{fieldErrors.username}</p>
              ) : null}
            </div>
            <div className="agent-identity-dialog__field">
              <label>
                <span>Display name (optional)</span>
                <input
                  type="text"
                  autoComplete="off"
                  value={displayName}
                  disabled={busy || Boolean(pendingRefresh)}
                  aria-invalid={fieldErrors.displayName ? "true" : undefined}
                  aria-describedby={fieldErrors.displayName ? `${titleId}-display-error` : undefined}
                  onChange={(event) => setDisplayName(event.currentTarget.value)}
                />
              </label>
              {fieldErrors.displayName ? (
                <p id={`${titleId}-display-error`} className="field-error">{fieldErrors.displayName}</p>
              ) : null}
            </div>
            <button
              className="button button-primary"
              type="submit"
              disabled={busy || Boolean(pendingRefresh)}
            >
              {busy && !pendingRefresh ? "Creating…" : "Create and add"}
            </button>
          </form>
        </section>

        <section className="agent-identity-dialog__section" aria-labelledby={`${titleId}-unowned`}>
          <div>
            <p className="eyebrow">Imported into this room</p>
            <h4 id={`${titleId}-unowned`}>Needs an owner</h4>
            <p>
              These identities are visible in this room only. Claiming requires room-manager access.
            </p>
          </div>
          {!canClaim && unownedIdentities.length > 0 ? (
            <p className="dashboard-note">A room manager must claim imported identities.</p>
          ) : null}
          {unownedIdentities.length === 0 ? (
            <p>No roster-visible identities need an owner.</p>
          ) : (
            <div className="agent-identity-dialog__identity-list">
              {unownedIdentities.map((identity) => (
                <div className="agent-identity-dialog__identity" key={identity.id}>
                  <div>
                    <strong>{identityName(identity)}</strong>
                    <span>@{identity.username}</span>
                  </div>
                  <button
                    className="button button-secondary"
                    type="button"
                    disabled={!canClaim || busy || Boolean(pendingRefresh)}
                    aria-label={`Claim ${identityName(identity)}`}
                    onClick={() => setClaimCandidate(identity)}
                  >
                    Claim
                  </button>
                </div>
              ))}
            </div>
          )}
        </section>

        {claimCandidate ? (
          <section className="agent-identity-dialog__confirm" aria-label="Confirm identity claim">
            <p>
              Claiming makes you the persistent owner of {identityName(claimCandidate)}.
            </p>
            <div>
              <button
                className="button button-secondary"
                type="button"
                disabled={busy}
                onClick={() => setClaimCandidate(null)}
              >
                Cancel
              </button>
              <button
                className="button button-primary"
                type="button"
                disabled={busy}
                onClick={() => void confirmClaim()}
              >
                {busy ? "Claiming…" : "Confirm claim"}
              </button>
            </div>
          </section>
        ) : null}
      </div>
    </div>
  );
}
