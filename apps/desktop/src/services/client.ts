/** Runtime adapter selection for the Henosis webview. */
import { FixtureHenosisClient } from "./fixtureClient";
import type { HenosisClient } from "./henosisClient";
import { TauriHenosisClient } from "./tauriClient";

/** Minimal global shape Tauri injects into its webview. */
interface TauriAwareWindow extends Window {
  /** Internal Tauri IPC marker absent from normal browsers and jsdom. */
  __TAURI_INTERNALS__?: unknown;
}

/** Detect whether the current page is running inside the Tauri webview. */
export function isTauriRuntime(): boolean {
  return Boolean((window as TauriAwareWindow).__TAURI_INTERNALS__);
}

/** Create the native client in production and an explicit fixture client in browsers. */
export function createHenosisClient(): HenosisClient {
  return isTauriRuntime()
    ? new TauriHenosisClient()
    : new FixtureHenosisClient();
}
