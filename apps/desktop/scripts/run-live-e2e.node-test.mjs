#!/usr/bin/env node
/** Deterministic process-lifecycle contracts for the live desktop E2E harness. */

import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  TAURI_DRIVER_PREFLIGHT_ARGUMENTS,
  diagnosticTail,
  formatHarnessDiagnostic,
  initializeHarnessDiagnostic,
  processGroupExists,
  redactDiagnosticContents,
  runCommand,
  spawnSupervisedProcess,
  terminateService,
} from "./run-live-e2e.mjs";

test(
  "the tauri-driver preflight uses a supported non-starting argument",
  /** Keep the executable preflight aligned with tauri-driver's strict CLI. */
  function usesSupportedTauriDriverPreflight() {
    assert.deepEqual(TAURI_DRIVER_PREFLIGHT_ARGUMENTS, ["--help"]);
  },
);

test(
  "the harness creates uploadable diagnostics before service startup",
  /** Prove a preflight failure still leaves one private regular file. */
  async function createsEarlyDiagnosticFile() {
    const testDirectory = await mkdtemp(join(tmpdir(), "henosis-e2e-log-test-"));
    const logPath = join(testDirectory, "harness.log");
    try {
      await initializeHarnessDiagnostic(logPath);

      assert.equal(
        await readFile(logPath, "utf8"),
        "stage=preflight\nresult=running\n",
      );
      const metadata = await stat(logPath);
      assert.equal(metadata.isFile(), true);
      assert.equal(metadata.mode & 0o777, 0o600);
    } finally {
      await rm(testDirectory, { recursive: true });
    }
  },
);

test(
  "early harness failures retain bounded redacted diagnostics",
  /** Prove failures before service logs exist still produce safe evidence. */
  function retainsSafeEarlyFailureEvidence() {
    const secret = "ephemeral-database-url";
    const diagnostic = formatHarnessDiagnostic(
      "preflight",
      new Error(`tauri-driver rejected ${secret}`),
      undefined,
      [secret],
    );

    assert.match(diagnostic, /^stage=preflight\nresult=failure\n/);
    assert.match(diagnostic, /run_error=tauri-driver rejected \[REDACTED\]/);
    assert.doesNotMatch(diagnostic, /ephemeral-database-url/);
    assert.ok(diagnostic.length <= 16_500);
  },
);

test(
  "retained diagnostics redact secrets before printing a bounded tail",
  /** Prove a failed live run cannot print credentials or unbounded logs. */
  function redactsAndBoundsDiagnostics() {
    const secret = "ephemeral-password";
    const redacted = redactDiagnosticContents(
      `${"x".repeat(20_000)} ${secret}`,
      [secret],
    );
    const tail = diagnosticTail(redacted);

    assert.doesNotMatch(tail, /ephemeral-password/);
    assert.match(tail, /\[REDACTED\]$/);
    assert.match(tail, /^\[earlier diagnostic output truncated\]/);
    assert.ok(tail.length <= 16_500);
  },
);

test(
  "the persistent supervisor removes descendants after its command exits",
  { skip: process.platform !== "linux" },
  /** Prove cleanup retains a live group identity while removing an orphan. */
  async function removesOrphanedDescendant() {
    const service = spawnSupervisedProcess(
      "orphaned-descendant fixture",
      "sh",
      ["-c", "sleep 30 </dev/null >/dev/null 2>&1 &"],
      { stdio: "ignore" },
    );
    try {
      await service.started;
      assert.deepEqual(await service.completion, { code: 0, signal: null });
      assert.equal(service.child.exitCode, null);
      assert.notEqual(service.child.pid, undefined);
      assert.equal(processGroupExists(service.child.pid), true);
      await terminateService(service);
      assert.equal(processGroupExists(service.child.pid), false);
    } finally {
      await terminateService(service);
    }
  },
);

test(
  "bounded commands terminate instead of waiting forever",
  { skip: process.platform !== "linux" },
  /** Prove a hung child is rejected by the harness-owned deadline. */
  async function rejectsHungCommand() {
    await assert.rejects(
      runCommand(
        process.execPath,
        ["-e", "setInterval(() => undefined, 1000)"],
        { stdio: "ignore", timeoutMs: 100 },
      ),
      /exceeded its 100ms timeout/,
    );
  },
);
