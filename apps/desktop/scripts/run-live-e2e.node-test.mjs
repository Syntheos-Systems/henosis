#!/usr/bin/env node
/** Deterministic process-lifecycle contracts for the live desktop E2E harness. */

import assert from "node:assert/strict";
import test from "node:test";
import {
  diagnosticTail,
  processGroupExists,
  redactDiagnosticContents,
  runCommand,
  spawnSupervisedProcess,
  terminateService,
} from "./run-live-e2e.mjs";

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
