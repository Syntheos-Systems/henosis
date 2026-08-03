#!/usr/bin/env node
/** Own the compiled Henosis desktop and Rift processes for one black-box E2E run. */

import { execFile, fork, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { once } from "node:events";
import {
  closeSync,
  constants as fileConstants,
  openSync,
} from "node:fs";
import {
  access,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath, pathToFileURL } from "node:url";

const execFileAsync = promisify(execFile);
const SCRIPT_PATH = fileURLToPath(import.meta.url);
const REPOSITORY_DIRECTORY = resolve(dirname(SCRIPT_PATH), "../../..");
const DESKTOP_DIRECTORY = join(REPOSITORY_DIRECTORY, "apps", "desktop");
const DESKTOP_MANIFEST = join(DESKTOP_DIRECTORY, "src-tauri", "Cargo.toml");
const LOOPBACK_HOST = "127.0.0.1";
const DRIVER_PORT = 4444;
const STARTUP_TIMEOUT_MS = 30_000;
const STOP_TIMEOUT_MS = 5_000;
const PREFLIGHT_TIMEOUT_MS = 10_000;
const METADATA_TIMEOUT_MS = 30_000;
const SERVER_BUILD_TIMEOUT_MS = 15 * 60_000;
const DESKTOP_BUILD_TIMEOUT_MS = 20 * 60_000;
const WDIO_TIMEOUT_MS = 3 * 60_000;
const HARNESS_TIMEOUT_MS = 44 * 60_000;
const SUPERVISOR_ARGUMENT = "--supervise-owned-process";
const CHILD_SECRET_NAMES = [
  "DATABASE_URL",
  "HENOSIS_RIFT_TEST_DATABASE_URL",
  "JWT_SECRET",
  "RIFT_BRIDGE_SECRET",
  "HENOSIS_E2E_PASSWORD",
];

process.umask(0o077);

/** Read one mandatory environment value without printing its contents. */
function requireEnvironment(name) {
  const value = process.env[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be set by the isolated test harness`);
  }
  return value;
}

/** Parse one loopback service port within the non-privileged TCP range. */
function parsePort(name, fallback) {
  const value = process.env[name] ?? String(fallback);
  const port = Number.parseInt(value, 10);
  if (!/^\d+$/.test(value) || port < 1024 || port > 65_535) {
    throw new Error(`${name} must be an integer between 1024 and 65535`);
  }
  return port;
}

/** Strip harness secrets before adding the exact values one child requires. */
function childEnvironment(overrides = {}) {
  const environment = { ...process.env };
  for (const name of CHILD_SECRET_NAMES) {
    delete environment[name];
  }
  return { ...environment, ...overrides };
}

/** Add an IPC channel to the requested standard-I/O contract. */
function supervisorStdio(stdio = "inherit") {
  if (stdio === "inherit") {
    return ["inherit", "inherit", "inherit", "ipc"];
  }
  if (stdio === "ignore") {
    return ["ignore", "ignore", "ignore", "ipc"];
  }
  if (Array.isArray(stdio) && stdio.length === 3) {
    return [...stdio, "ipc"];
  }
  throw new Error("supervised process stdio must describe descriptors 0 through 2");
}

/** Start a persistent process-group leader and one command beneath it. */
function spawnSupervisedProcess(label, command, args, options = {}) {
  const supervisor = fork(
    SCRIPT_PATH,
    [SUPERVISOR_ARGUMENT, command, ...args],
    {
      cwd: options.cwd,
      env: options.env,
      detached: true,
      stdio: supervisorStdio(options.stdio),
    },
  );
  let resolveStarted;
  let rejectStarted;
  let resolveCompletion;
  const service = {
    label,
    child: supervisor,
    commandExit: undefined,
    started: new Promise((resolveStart, rejectStart) => {
      resolveStarted = resolveStart;
      rejectStarted = rejectStart;
    }),
    completion: new Promise((resolveExit) => {
      resolveCompletion = resolveExit;
    }),
  };

  /** Record one terminal command result exactly once. */
  function finishCommand(result) {
    if (service.commandExit !== undefined) {
      return;
    }
    service.commandExit = result;
    resolveCompletion(result);
  }

  supervisor.on("message", (message) => {
    if (message?.type === "spawned") {
      resolveStarted();
      return;
    }
    if (message?.type === "exit") {
      finishCommand({ code: message.code, signal: message.signal });
      return;
    }
    if (message?.type === "error") {
      const detail = String(message.message ?? "unknown spawn error").slice(0, 300);
      const error = new Error(`${label} failed to start: ${detail}`);
      rejectStarted(error);
      finishCommand({ error });
    }
  });
  supervisor.once("error", (error) => {
    rejectStarted(error);
    finishCommand({ error });
  });
  supervisor.once("exit", (code, signal) => {
    if (service.commandExit === undefined) {
      const error = new Error(
        `${label} supervisor exited ${signal === null ? `with status ${code}` : `from ${signal}`}`,
      );
      rejectStarted(error);
      finishCommand({ error });
    }
  });
  return service;
}

/** Send one bounded lifecycle event from a supervisor to its owner. */
function sendSupervisorEvent(message) {
  if (process.send === undefined || !process.connected) {
    return;
  }
  try {
    process.send(message, (error) => {
      if (
        error !== null &&
        error !== undefined &&
        error.code !== "ERR_IPC_CHANNEL_CLOSED"
      ) {
        console.error("Supervisor IPC write failed");
      }
    });
  } catch (error) {
    if (!(error instanceof Error) || error.code !== "ERR_IPC_CHANNEL_CLOSED") {
      throw error;
    }
  }
}

/** Supervise one command as the persistent leader of its isolated process group. */
function superviseOwnedProcess() {
  const [command, ...args] = process.argv.slice(3);
  if (command === undefined || command.length === 0) {
    throw new Error("The process supervisor requires a command");
  }

  /** Keep the group leader alive while its command handles graceful termination. */
  function ignoreTermination() {}

  /** Perform an identity-safe stop when the owning harness requests it. */
  function handleStopRequest(message) {
    if (message?.type === "stop") {
      void stopSupervisedGroup().catch(reportFatalError);
    }
  }

  /** Prevent an abandoned command group when the owning harness disappears. */
  function handleOwnerDisconnect() {
    signalOwnProcessGroup("SIGKILL");
  }

  process.on("SIGTERM", ignoreTermination);
  process.on("message", handleStopRequest);
  process.once("disconnect", handleOwnerDisconnect);

  const child = spawn(command, args, { stdio: "inherit" });
  child.once("spawn", () => {
    sendSupervisorEvent({ type: "spawned" });
  });
  child.once("error", (error) => {
    sendSupervisorEvent({
      type: "error",
      message: error.message.slice(0, 300),
    });
  });
  child.once("exit", (code, signal) => {
    sendSupervisorEvent({ type: "exit", code, signal });
  });
}

/** Run one bounded child command and reject on signals or nonzero status. */
async function runCommand(command, args, options = {}) {
  const timeoutMs = options.timeoutMs;
  if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) {
    throw new Error(`A positive timeout is required for ${command}`);
  }
  if (process.exitCode === 130 || process.exitCode === 143) {
    throw new Error(`Refusing to start ${command} after interruption`);
  }

  const service = spawnSupervisedProcess(
    options.label ?? command,
    command,
    args,
    {
      cwd: options.cwd,
      env: options.env,
      stdio: options.stdio ?? "inherit",
    },
  );
  options.ownedServices?.push(service);
  void service.started.catch(() => undefined);
  let timeoutHandle;
  const timeout = new Promise((resolveTimeout) => {
    timeoutHandle = setTimeout(() => resolveTimeout(null), timeoutMs);
  });

  try {
    const result = await Promise.race([service.completion, timeout]);
    if (result === null) {
      await terminateService(service);
      throw new Error(`${service.label} exceeded its ${timeoutMs}ms timeout`);
    }

    await terminateService(service);
    if (result.error !== undefined) {
      throw result.error;
    }
    if (result.code !== 0) {
      throw new Error(
        `${service.label} exited ${result.signal === null ? `with status ${result.code}` : `from ${result.signal}`}`,
      );
    }
  } catch (error) {
    await terminateService(service);
    throw error;
  } finally {
    clearTimeout(timeoutHandle);
    if (options.ownedServices !== undefined) {
      const index = options.ownedServices.indexOf(service);
      if (index !== -1) {
        options.ownedServices.splice(index, 1);
      }
    }
  }
}

/** Capture one metadata command without exposing its environment or arguments. */
async function captureCommand(command, args, options = {}) {
  const result = await execFileAsync(command, args, {
    cwd: options.cwd,
    env: options.env,
    encoding: "utf8",
    maxBuffer: 4 * 1024 * 1024,
    timeout: METADATA_TIMEOUT_MS,
  });
  return result.stdout;
}

/** Resolve Cargo's effective target directory for one manifest and toolchain. */
async function cargoTargetDirectory(manifestPath, environment) {
  const args = ["metadata", "--format-version", "1", "--no-deps"];
  if (manifestPath !== null) {
    args.push("--manifest-path", manifestPath);
  }
  const stdout = await captureCommand("cargo", args, {
    cwd: REPOSITORY_DIRECTORY,
    env: environment,
  });
  const metadata = JSON.parse(stdout);
  if (
    typeof metadata.target_directory !== "string" ||
    metadata.target_directory.length === 0
  ) {
    throw new Error("Cargo metadata did not return a target directory");
  }
  return metadata.target_directory;
}

/** Require one built binary to exist and carry execute permission. */
async function requireExecutable(path, label) {
  try {
    await access(path, fileConstants.X_OK);
  } catch {
    throw new Error(`${label} binary was not produced at ${path}`);
  }
}

/** Prove one loopback port is free before starting an owned service. */
async function requireAvailablePort(port, label) {
  await new Promise((resolvePort, rejectPort) => {
    const server = createServer();
    server.unref();
    server.once("error", () => {
      rejectPort(new Error(`${label} port ${port} is already in use`));
    });
    server.listen({ host: LOOPBACK_HOST, port, exclusive: true }, () => {
      server.close((error) => {
        if (error) {
          rejectPort(error);
        } else {
          resolvePort();
        }
      });
    });
  });
}

/** Start one detached service whose complete output stays in a private log. */
function spawnService(label, command, args, options) {
  const logDescriptor = openSync(
    options.logPath,
    fileConstants.O_WRONLY | fileConstants.O_CREAT | fileConstants.O_APPEND,
    0o600,
  );
  let service;
  try {
    service = spawnSupervisedProcess(label, command, args, {
      cwd: options.cwd,
      env: options.env,
      stdio: ["ignore", logDescriptor, logDescriptor],
    });
  } finally {
    closeSync(logDescriptor);
  }
  return service;
}

/** Poll one HTTP endpoint until it responds or its owned service exits. */
async function waitForHttp(service, url, timeoutMs = STARTUP_TIMEOUT_MS) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    if (service.commandExit !== undefined) {
      if (service.commandExit.error !== undefined) {
        throw service.commandExit.error;
      }
      throw new Error(
        `${service.label} exited ${service.commandExit.signal === null ? `with status ${service.commandExit.code}` : `from ${service.commandExit.signal}`} before becoming ready`,
      );
    }
    if (service.child.exitCode !== null || service.child.signalCode !== null) {
      throw new Error(`${service.label} supervisor exited before it became ready`);
    }
    try {
      const response = await fetch(url, {
        redirect: "error",
        signal: AbortSignal.timeout(1_000),
      });
      await response.body?.cancel();
      return;
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolvePoll) => setTimeout(resolvePoll, 100));
  }
  throw new Error(
    `${service.label} did not become ready within ${timeoutMs}ms: ${lastError instanceof Error ? lastError.message : "unknown transport error"}`,
  );
}

/** Report whether an owned Unix process group still has any members. */
function processGroupExists(processGroupId) {
  try {
    process.kill(-processGroupId, 0);
    return true;
  } catch (error) {
    if (error instanceof Error && error.code === "ESRCH") {
      return false;
    }
    throw error;
  }
}

/** Signal the caller's own process group while its leader identity is certain. */
function signalOwnProcessGroup(signal) {
  try {
    process.kill(-process.pid, signal);
    return true;
  } catch (error) {
    if (error instanceof Error && error.code === "ESRCH") {
      return false;
    }
    throw error;
  }
}

/** Read the Linux process IDs that still belong to one process group. */
async function processGroupMembers(processGroupId) {
  const entries = await readdir("/proc", { withFileTypes: true });
  const members = [];
  await Promise.all(
    entries
      .filter((entry) => entry.isDirectory() && /^\d+$/.test(entry.name))
      .map(async (entry) => {
        try {
          const stat = await readFile(`/proc/${entry.name}/stat`, "utf8");
          const commandEnd = stat.lastIndexOf(")");
          if (commandEnd === -1) {
            return;
          }
          const fields = stat.slice(commandEnd + 2).split(" ");
          if (Number.parseInt(fields[2], 10) === processGroupId) {
            members.push(Number.parseInt(entry.name, 10));
          }
        } catch (error) {
          if (error instanceof Error && error.code === "ENOENT") {
            return;
          }
          throw error;
        }
      }),
  );
  return members;
}

/** Poll until the supervisor is the sole member of its own process group. */
async function waitForOnlySupervisor(timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const members = await processGroupMembers(process.pid);
    if (members.length === 1 && members[0] === process.pid) {
      return true;
    }
    await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
  }
  const members = await processGroupMembers(process.pid);
  return members.length === 1 && members[0] === process.pid;
}

/** Terminate every process in the supervisor's group while its identity is live. */
async function stopSupervisedGroup() {
  signalOwnProcessGroup("SIGTERM");
  try {
    await waitForOnlySupervisor(STOP_TIMEOUT_MS);
  } finally {
    signalOwnProcessGroup("SIGKILL");
  }
}

/** Wait a bounded interval for one supervisor process to exit. */
async function waitForSupervisorExit(child, timeoutMs) {
  if (child.exitCode !== null || child.signalCode !== null) {
    return true;
  }
  try {
    await once(child, "exit", { signal: AbortSignal.timeout(timeoutMs) });
    return true;
  } catch (error) {
    if (error instanceof Error && error.name === "AbortError") {
      return false;
    }
    throw error;
  }
}

/** Ask a live supervisor to perform identity-safe group termination itself. */
async function requestSupervisorStop(child) {
  await new Promise((resolveRequest, rejectRequest) => {
    child.send({ type: "stop" }, (error) => {
      if (error === null || error === undefined) {
        resolveRequest();
      } else {
        rejectRequest(error);
      }
    });
  });
}

/** Observe group disappearance without ever signaling a possibly reused ID. */
async function waitForProcessGroupExit(processGroupId, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!processGroupExists(processGroupId)) {
      return true;
    }
    await new Promise((resolvePoll) => setTimeout(resolvePoll, 50));
  }
  return !processGroupExists(processGroupId);
}

/** Stop and reap a service through its persistent process-group supervisor. */
async function terminateServiceOnce(service) {
  const { child, label } = service;
  if (child.pid === undefined) {
    return;
  }
  if (child.exitCode !== null || child.signalCode !== null) {
    if (processGroupExists(child.pid)) {
      throw new Error(
        `${label} supervisor exited while its group remained; refusing an unsafe numeric signal`,
      );
    }
    return;
  }

  await requestSupervisorStop(child);
  if (!(await waitForSupervisorExit(child, STOP_TIMEOUT_MS * 2))) {
    throw new Error(`${label} supervisor did not stop`);
  }
  if (!(await waitForProcessGroupExit(child.pid, STOP_TIMEOUT_MS))) {
    throw new Error(`${label} process group did not stop`);
  }
}

/** Make repeated or concurrent cleanup calls share one exact termination. */
async function terminateService(service) {
  service.terminationPromise ??= terminateServiceOnce(service);
  await service.terminationPromise;
}

/** Stop every owned service in reverse startup order while preserving cleanup errors. */
async function terminateServices(services) {
  let firstError;
  for (const service of [...services].reverse()) {
    try {
      await terminateService(service);
    } catch (error) {
      firstError ??= error;
    }
  }
  if (firstError !== undefined) {
    throw firstError;
  }
}

/** Replace ephemeral credentials in one retained diagnostic log. */
async function redactLog(logPath, secrets) {
  let contents;
  try {
    contents = await readFile(logPath, "utf8");
  } catch (error) {
    if (error instanceof Error && error.code === "ENOENT") {
      return;
    }
    throw error;
  }
  let redacted = contents;
  for (const secret of secrets) {
    redacted = redacted.replaceAll(secret, "[REDACTED]");
  }
  await writeFile(logPath, redacted, { encoding: "utf8", mode: 0o600 });
}

/** Re-enter this runner beneath Xvfb when Linux has no display server. */
async function runUnderVirtualDisplay() {
  const databaseUrl = requireEnvironment("DATABASE_URL");
  const services = [];
  let cleanupPromise;

  /** Start idempotent cleanup for the virtual-display process group. */
  function startCleanup() {
    cleanupPromise ??= terminateServices(services);
    return cleanupPromise;
  }

  /** Stop the virtual display after a user interrupt. */
  function handleInterrupt() {
    process.exitCode = 130;
    void startCleanup().catch(() => undefined);
  }

  /** Stop the virtual display after an external termination request. */
  function handleTermination() {
    process.exitCode = 143;
    void startCleanup().catch(() => undefined);
  }

  process.once("SIGINT", handleInterrupt);
  process.once("SIGTERM", handleTermination);
  try {
    await runCommand("xvfb-run", ["-a", process.execPath, SCRIPT_PATH], {
      cwd: REPOSITORY_DIRECTORY,
      env: childEnvironment({
        DATABASE_URL: databaseUrl,
        HENOSIS_E2E_UNDER_XVFB: "1",
      }),
      label: "virtual-display E2E harness",
      ownedServices: services,
      timeoutMs: HARNESS_TIMEOUT_MS,
    });
  } finally {
    await startCleanup();
    process.off("SIGINT", handleInterrupt);
    process.off("SIGTERM", handleTermination);
  }
}

/** Build and prove the complete desktop-to-Rift conversation path. */
async function runLiveE2e() {
  const databaseUrl = requireEnvironment("DATABASE_URL");
  const riftPort = parsePort("HENOSIS_E2E_RIFT_PORT", 4010);
  const suffix = randomBytes(6).toString("hex");
  const testDirectory = await mkdtemp(join(tmpdir(), "henosis-desktop-e2e-"));
  const artifactRoot = process.env.HENOSIS_E2E_ARTIFACT_DIR
    ? resolve(process.env.HENOSIS_E2E_ARTIFACT_DIR)
    : join(testDirectory, "artifacts");
  const artifactDirectory = join(artifactRoot, `run-${suffix}`);
  const xdgDataDirectory = join(testDirectory, "xdg-data");
  const xdgConfigDirectory = join(testDirectory, "xdg-config");
  const xdgCacheDirectory = join(testDirectory, "xdg-cache");
  const uploadDirectory = join(testDirectory, "uploads");
  const riftLog = join(artifactDirectory, "rift.log");
  const driverLog = join(artifactDirectory, "tauri-driver.log");
  const jwtSecret = randomBytes(32).toString("hex");
  const bridgeSecret = randomBytes(32).toString("hex");
  const password = randomBytes(18).toString("base64url");
  const services = [];
  let cleanupPromise;
  let passed = false;

  await Promise.all([
    mkdir(artifactDirectory, { recursive: true, mode: 0o700 }),
    mkdir(xdgDataDirectory, { recursive: true, mode: 0o700 }),
    mkdir(xdgConfigDirectory, { recursive: true, mode: 0o700 }),
    mkdir(xdgCacheDirectory, { recursive: true, mode: 0o700 }),
    mkdir(uploadDirectory, { recursive: true, mode: 0o700 }),
  ]);

  /** Start idempotent cleanup for every service owned by this invocation. */
  function startCleanup() {
    cleanupPromise ??= terminateServices(services);
    return cleanupPromise;
  }

  /** Begin cleanup and retain the conventional interrupt exit status. */
  function handleInterrupt() {
    process.exitCode = 130;
    void startCleanup().catch(() => undefined);
  }

  /** Begin cleanup and retain the conventional termination exit status. */
  function handleTermination() {
    process.exitCode = 143;
    void startCleanup().catch(() => undefined);
  }

  process.once("SIGINT", handleInterrupt);
  process.once("SIGTERM", handleTermination);

  try {
    await requireAvailablePort(riftPort, "Rift");
    await requireAvailablePort(DRIVER_PORT, "tauri-driver");
    await runCommand("tauri-driver", ["--version"], {
      ownedServices: services,
      stdio: "ignore",
      timeoutMs: PREFLIGHT_TIMEOUT_MS,
    });
    await runCommand("WebKitWebDriver", ["--help"], {
      ownedServices: services,
      stdio: "ignore",
      timeoutMs: PREFLIGHT_TIMEOUT_MS,
    });

    const serverBuildEnvironment = childEnvironment({
      RUSTUP_TOOLCHAIN:
        process.env.HENOSIS_E2E_SERVER_TOOLCHAIN ?? "1.94.0",
    });
    const desktopBuildEnvironment = childEnvironment({
      RUSTUP_TOOLCHAIN:
        process.env.HENOSIS_E2E_DESKTOP_TOOLCHAIN ?? "1.88.0",
    });

    console.log("Building the Rift server for live desktop E2E");
    await runCommand(
      "cargo",
      ["build", "--locked", "-p", "henosis-rift-server"],
      {
        cwd: REPOSITORY_DIRECTORY,
        env: serverBuildEnvironment,
        label: "Rift build",
        ownedServices: services,
        timeoutMs: SERVER_BUILD_TIMEOUT_MS,
      },
    );
    const serverTargetDirectory = await cargoTargetDirectory(
      null,
      serverBuildEnvironment,
    );
    const riftBinary = join(serverTargetDirectory, "debug", "henosis-rift-server");
    await requireExecutable(riftBinary, "Rift");

    console.log("Building the compiled Tauri application for live desktop E2E");
    await runCommand(
      "pnpm",
      ["--dir", DESKTOP_DIRECTORY, "tauri", "build", "--debug", "--no-bundle"],
      {
        cwd: REPOSITORY_DIRECTORY,
        env: desktopBuildEnvironment,
        label: "Tauri build",
        ownedServices: services,
        timeoutMs: DESKTOP_BUILD_TIMEOUT_MS,
      },
    );
    const desktopTargetDirectory = await cargoTargetDirectory(
      DESKTOP_MANIFEST,
      desktopBuildEnvironment,
    );
    const desktopBinary = join(desktopTargetDirectory, "debug", "henosis");
    await requireExecutable(desktopBinary, "Henosis desktop");

    const riftEndpoint = `http://${LOOPBACK_HOST}:${riftPort}`;
    const runtimeEnvironment = childEnvironment({
      XDG_DATA_HOME: xdgDataDirectory,
      XDG_CONFIG_HOME: xdgConfigDirectory,
      XDG_CACHE_HOME: xdgCacheDirectory,
    });
    const riftEnvironment = {
      ...runtimeEnvironment,
      DATABASE_URL: databaseUrl,
      JWT_SECRET: jwtSecret,
      RIFT_BRIDGE_SECRET: bridgeSecret,
      LISTEN_ADDR: `${LOOPBACK_HOST}:${riftPort}`,
      UPLOAD_DIR: uploadDirectory,
      RUST_LOG: "henosis_rift_server=info,tower_http=warn",
    };

    const riftService = spawnService("Rift", riftBinary, [], {
      cwd: REPOSITORY_DIRECTORY,
      env: riftEnvironment,
      logPath: riftLog,
    });
    services.push(riftService);
    await riftService.started;
    await waitForHttp(riftService, `${riftEndpoint}/api/auth/login`);

    const driverService = spawnService("tauri-driver", "tauri-driver", [], {
      cwd: DESKTOP_DIRECTORY,
      env: runtimeEnvironment,
      logPath: driverLog,
    });
    services.push(driverService);
    await driverService.started;
    await waitForHttp(
      driverService,
      `http://${LOOPBACK_HOST}:${DRIVER_PORT}/status`,
    );

    const testEnvironment = {
      ...runtimeEnvironment,
      HENOSIS_E2E_APP_BINARY: desktopBinary,
      HENOSIS_E2E_ARTIFACT_DIR: artifactDirectory,
      HENOSIS_E2E_DRIVER_PORT: String(DRIVER_PORT),
      HENOSIS_E2E_RIFT_URL: riftEndpoint,
      HENOSIS_E2E_USERNAME: `e2e_${suffix}`,
      HENOSIS_E2E_PASSWORD: password,
      HENOSIS_E2E_EMAIL: `e2e-${suffix}@example.invalid`,
      HENOSIS_E2E_SERVER_NAME: `Henosis E2E ${suffix}`,
      HENOSIS_E2E_SEED_MESSAGE: `seed-${suffix}`,
      HENOSIS_E2E_LIVE_MESSAGE: `live-${suffix}`,
      HENOSIS_E2E_UI_MESSAGE: `ui-${suffix}`,
    };

    console.log("Running the black-box Tauri and Rift conversation proof");
    await runCommand(
      "pnpm",
      [
        "--dir",
        DESKTOP_DIRECTORY,
        "exec",
        "wdio",
        "run",
        "e2e/wdio.conf.mjs",
      ],
      {
        cwd: REPOSITORY_DIRECTORY,
        env: testEnvironment,
        label: "WebdriverIO live conversation",
        ownedServices: services,
        timeoutMs: WDIO_TIMEOUT_MS,
      },
    );
    if (process.exitCode === 130 || process.exitCode === 143) {
      throw new Error("Live desktop E2E was interrupted");
    }
    passed = true;
    console.log("Live desktop E2E passed");
  } finally {
    let cleanupError;
    try {
      await startCleanup();
    } catch (error) {
      cleanupError = error;
    }
    process.off("SIGINT", handleInterrupt);
    process.off("SIGTERM", handleTermination);
    await Promise.all([
      redactLog(riftLog, [databaseUrl, jwtSecret, bridgeSecret, password]),
      redactLog(driverLog, [databaseUrl, jwtSecret, bridgeSecret, password]),
    ]);
    if (passed && cleanupError === undefined) {
      await rm(testDirectory, { recursive: true });
    } else {
      console.error(`Live E2E diagnostics retained at ${artifactDirectory}`);
    }
    if (cleanupError !== undefined) {
      throw cleanupError;
    }
  }
}

/** Run the platform wrapper or the live E2E implementation. */
async function main() {
  if (process.platform !== "linux") {
    throw new Error("Live desktop E2E currently requires Linux process isolation");
  }
  if (!process.env.DISPLAY && process.env.HENOSIS_E2E_UNDER_XVFB !== "1") {
    await runUnderVirtualDisplay();
    return;
  }
  await runLiveE2e();
}

/** Report one safe top-level failure without dumping child environments. */
function reportFatalError(error) {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode ||= 1;
}

/** Report whether Node launched this module as the requested command. */
function isMainModule() {
  return (
    process.argv[1] !== undefined &&
    import.meta.url === pathToFileURL(resolve(process.argv[1])).href
  );
}

if (isMainModule()) {
  if (process.argv[2] === SUPERVISOR_ARGUMENT) {
    try {
      superviseOwnedProcess();
    } catch (error) {
      reportFatalError(error);
    }
  } else {
    void main().catch(reportFatalError);
  }
}

export {
  processGroupExists,
  runCommand,
  spawnSupervisedProcess,
  terminateService,
};
