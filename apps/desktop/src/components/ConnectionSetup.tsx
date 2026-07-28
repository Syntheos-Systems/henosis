/** First-run Rift connection screen for the Henosis desktop application. */
import { FormEvent, useState } from "react";
import { ArrowRight, KeyRound, Network, ShieldCheck } from "lucide-react";
import type {
  ConnectionProfile,
  RiftConnectionInput,
} from "../services/henosisClient";

/** Inputs and callbacks required by the first-run connection form. */
export interface ConnectionSetupProps {
  /** Saved non-secret fields used to prefill the form. */
  profile?: ConnectionProfile;
  /** True while the native process authenticates and aggregates rooms. */
  busy: boolean;
  /** Safe error message from the native boundary. */
  error?: string;
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
  const [endpoint, setEndpoint] = useState(
    profile?.endpoint ?? "http://127.0.0.1:4010",
  );
  const [username, setUsername] = useState(profile?.username ?? "");
  const [password, setPassword] = useState("");

  /** Submit credentials directly to the native client boundary. */
  async function handleSubmit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    await onConnect({
      endpoint: endpoint.trim(),
      username: username.trim(),
      password,
    });
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
        <div className="connection-card-header">
          <span className="connection-step">01</span>
          <div>
            <p className="eyebrow">Rift identity</p>
            <h2>Connect to a room service</h2>
          </div>
        </div>

        <form onSubmit={handleSubmit} className="connection-form">
          <div className="field">
            <label htmlFor="rift-endpoint">Rift endpoint</label>
            <input
              id="rift-endpoint"
              name="endpoint"
              type="url"
              inputMode="url"
              autoCapitalize="none"
              autoCorrect="off"
              spellCheck="false"
              value={endpoint}
              onChange={(event) => setEndpoint(event.target.value)}
              placeholder="http://127.0.0.1:4010"
              aria-describedby="endpoint-help"
              required
            />
            <p id="endpoint-help">Use the service root, without an API path.</p>
          </div>

          <div className="field-grid">
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
                required
              />
            </div>

            <div className="field">
              <label htmlFor="rift-password">Password</label>
              <input
                id="rift-password"
                name="password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(event) => setPassword(event.target.value)}
                required
              />
            </div>
          </div>

          {error ? (
            <div className="form-error" role="alert">
              <span aria-hidden="true">!</span>
              <p>{error}</p>
            </div>
          ) : null}

          <button className="button button-primary connect-button" type="submit" disabled={busy}>
            <span>{busy ? "Finding your rooms…" : "Connect and view rooms"}</span>
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
