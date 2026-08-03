/** WebdriverIO contract for the compiled Henosis Tauri application. */

import { mkdir } from "node:fs/promises";
import { join } from "node:path";

/** Read one required test setting without printing its contents. */
function requireEnvironment(name) {
  const value = process.env[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} is required by the live E2E harness`);
  }
  return value;
}

/** Parse the fixed local WebDriver port supplied by the process harness. */
function driverPort() {
  const value = requireEnvironment("HENOSIS_E2E_DRIVER_PORT");
  const port = Number.parseInt(value, 10);
  if (!/^\d+$/.test(value) || port < 1024 || port > 65_535) {
    throw new Error("HENOSIS_E2E_DRIVER_PORT must be a non-privileged TCP port");
  }
  return port;
}

const artifactDirectory = requireEnvironment("HENOSIS_E2E_ARTIFACT_DIR");

/** Capture the controlled application on failure without replacing the test error. */
async function captureFailureScreenshot(_test, _context, result) {
  if (result.passed) {
    return;
  }
  try {
    await mkdir(artifactDirectory, { recursive: true, mode: 0o700 });
    await browser.saveScreenshot(join(artifactDirectory, "failure.png"));
  } catch (error) {
    console.error(
      `Could not capture the failed Tauri window: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

export const config = {
  runner: "local",
  specs: ["./e2e/live-conversation.e2e.mjs"],
  maxInstances: 1,
  capabilities: [
    {
      maxInstances: 1,
      "tauri:options": {
        application: requireEnvironment("HENOSIS_E2E_APP_BINARY"),
      },
    },
  ],
  logLevel: "error",
  bail: 0,
  hostname: "127.0.0.1",
  port: driverPort(),
  waitforTimeout: 20_000,
  connectionRetryTimeout: 120_000,
  connectionRetryCount: 1,
  outputDir: artifactDirectory,
  framework: "mocha",
  reporters: ["spec"],
  mochaOpts: {
    ui: "bdd",
    timeout: 120_000,
  },
  afterTest: captureFailureScreenshot,
};
