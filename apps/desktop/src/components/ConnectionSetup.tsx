/** First-run Rift connection screen for the Henosis desktop application. */
import { FormEvent, useEffect, useRef, useState } from "react";
import { ArrowRight, KeyRound, Network, ShieldCheck } from "lucide-react";
import type {
  ConnectionProfile,
  HenosisClientError,
  RiftConnectionInput,
} from "../services/henosisClient";

/** Default listener used only when an operator already runs Rift locally. */
const LOCAL_RIFT_ENDPOINT = "http://127.0.0.1:3200";

/** Inputs and callbacks required by the first-run connection form. */
export interface ConnectionSetupProps {
  /** Saved non-secret fields used to prefill the form. */
  profile?: ConnectionProfile;
  /** True while the native process authenticates and aggregates rooms. */
  busy: boolean;
  /** Safe structured error from the native boundary. */
  error?: HenosisClientError;
  /** Authenticate through the native Henosis client. */
  onConnect(input: RiftConnectionInput): Promise<void>;
}

/** Render the complete GUI path for connecting Henosis to Rift. */
export function ConnectionSetup({
  profile,
  busy,
  error,
  onConnect,
}: ConnectionSetupProps) {
  const [endpoint, setEndpoint] = useState(profile?.endpoint ?? "");
  const [username, setUsername] = useState(profile?.username ?? "");
  const [password, setPassword] = useState("");
  const endpointInput = useRef<HTMLInputElement>(null);
  const passwordInput = useRef<HTMLInputElement>(null);
  const formError = useRef<HTMLDivElement>(null);
  const endpointError =
    error?.kind === "network" ||
    error?.kind === "validation" ||
    error?.kind === "connection-required"
      ? error.message
      : undefined;
  const accountError =
    error?.kind === "authentication" ? error.message : undefined;
  const generalError =
    error && !endpointError && !accountError ? error.message : undefined;

  /** Move keyboard focus to the control that can resolve the latest failure. */
  useEffect(() => {
    if (endpointError) {
      endpointInput.current?.focus();
    } else if (accountError) {
      passwordInput.current?.focus();
    } else if (generalError) {
      formError.current?.focus();
    }
  }, [accountError, endpointError, generalError]);

  /** Submit credentials directly to the native client boundary. */
  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    try {
      await onConnect({
        endpoint: endpoint.trim(),
        username: username.trim(),
        password,
      });
    } catch {
      setPassword("");
    }
  }

  /** Fill the listener default without implying that Henosis starts Rift. */
  function handleLocalRift() {
    setEndpoint(LOCAL_RIFT_ENDPOINT);
    endpointInput.current?.focus();
  }

  return (
    <main className="connection-page">
      <div className="connection-atmosphere" aria-hidden="true">
        <span />
        <span />
        <span />
      </div>

      <section className="connection-intro" aria-labelledby="connection-title">
        <p className="eyebrow">First connection</p>
        <h1 id="connection-title">Bring your rooms into view.</h1>
        <p className="connection-lede">
          Henosis opens where people and agents actually meet. Connect a Rift
          service to see every room, newest activity first.
        </p>

        <ol className="connection-assurances">
          <li>
            <Network aria-hidden="true" />
            <span>
              <strong>One visible doorway</strong>
              Rooms, work, memory, and governance stay inside Henosis.
            </span>
          </li>
          <li>
            <KeyRound aria-hidden="true" />
            <span>
              <strong>Native token custody</strong>
              Your password and tokens never enter browser storage.
            </span>
          </li>
          <li>
            <ShieldCheck aria-hidden="true" />
            <span>
              <strong>Honest connection state</strong>
              Live and cached room data are always labeled.
            </span>
          </li>
        </ol>
      </section>

      <section className="connection-card" aria-label="Rift connection">
        <nav className="connection-progress" aria-label="Setup progress">
          <ol>
            <li className="is-complete">
              <span aria-hidden="true">1</span>
              <div>
                <strong>Install</strong>
                <small>Application ready</small>
              </div>
            </li>
            <li className="is-current" aria-current="step">
              <span aria-hidden="true">2</span>
              <div>
                <strong>Connect</strong>
                <small>Current step</small>
              </div>
            </li>
            <li>
              <span aria-hidden="true">3</span>
              <div>
                <strong>Rooms</strong>
                <small>Next</small>
              </div>
            </li>
          </ol>
        </nav>

        <div className="connection-card-header">
          <span className="connection-step">01</span>
          <div>
            <p className="eyebrow">Rift identity</p>
            <h2>Connect to a room service</h2>
          </div>
        </div>

        <aside className="connection-prerequisite" aria-labelledby="rift-prerequisite-title">
          <p className="eyebrow">Before you connect</p>
          <h3 id="rift-prerequisite-title">Have your Rift details ready.</h3>
          <p>
            You need a service address and account from your Rift operator.
            Henosis desktop does not install or start Rift.
          </p>
          <button
            className="local-rift-button"
            type="button"
            onClick={handleLocalRift}
            disabled={busy}
          >
            Use an already-running local Rift
          </button>
        </aside>

        <form onSubmit={handleSubmit} className="connection-form" aria-busy={busy}>
          <div className="field">
            <label htmlFor="rift-endpoint">Rift endpoint</label>
            <input
              ref={endpointInput}
              id="rift-endpoint"
              name="endpoint"
              type="url"
              inputMode="url"
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck="false"
              value={endpoint}
              onChange={(event) => setEndpoint(event.target.value)}
              placeholder="https://rift.example.com"
              aria-describedby={
                endpointError ? "endpoint-help endpoint-error" : "endpoint-help"
              }
              aria-invalid={endpointError ? true : undefined}
              disabled={busy}
              required
            />
            <p id="endpoint-help">Use the service root, without an API path.</p>
            {endpointError ? (
              <p id="endpoint-error" className="field-error" role="alert">
                {endpointError}
              </p>
            ) : null}
          </div>

          <div className="field">
            <label htmlFor="rift-username">Username</label>
            <input
              id="rift-username"
              name="username"
              type="text"
              autoComplete="username"
              autoCapitalize="none"
              value={username}
              onChange={(event) => setUsername(event.target.value)}
              aria-describedby={accountError ? "account-error" : undefined}
              aria-invalid={accountError ? true : undefined}
              disabled={busy}
              required
            />
          </div>

          <div className="field">
            <label htmlFor="rift-password">Password</label>
            <input
              ref={passwordInput}
              id="rift-password"
              name="password"
              type="password"
              autoComplete="current-password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              aria-describedby={accountError ? "account-error" : undefined}
              aria-invalid={accountError ? true : undefined}
              disabled={busy}
              required
            />
            {accountError ? (
              <p id="account-error" className="field-error" role="alert">
                {accountError}
              </p>
            ) : null}
          </div>

          {generalError ? (
            <div
              ref={formError}
              className="form-error"
              role="alert"
              tabIndex={-1}
            >
              <span aria-hidden="true">!</span>
              <p>{generalError}</p>
            </div>
          ) : null}

          <button className="button button-primary connect-button" type="submit" disabled={busy}>
            <span>{busy ? "Finding your rooms…" : "Connect and open rooms"}</span>
            <ArrowRight aria-hidden="true" />
          </button>
        </form>

        <p className="native-boundary-note">
          Authentication travels from this form directly into the Henosis
          native process. The webview receives only your display identity and
          room summaries.
        </p>
      </section>
    </main>
  );
}
