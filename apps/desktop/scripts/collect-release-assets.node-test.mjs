/** Node-native tests for deterministic Tauri release artifact collection. */

import assert from "node:assert/strict";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import test from "node:test";

import { collectReleaseAssets } from "./collect-release-assets.mjs";

/** Create one isolated collector test directory. */
async function createTestDirectory() {
  return mkdtemp(join(tmpdir(), "henosis-desktop-assets-"));
}

test("collects required installers under stable release names", async (context) => {
  /** Isolated directory removed after this test. */
  const testDirectory = await createTestDirectory();
  context.after(() => rm(testDirectory, { recursive: true, force: true }));
  /** Nested Tauri bundle source used by the fixture. */
  const sourceDirectory = join(testDirectory, "bundle", "linux");
  /** Empty destination for stable release names. */
  const outputDirectory = join(testDirectory, "release");

  await mkdir(sourceDirectory, { recursive: true });
  await writeFile(join(sourceDirectory, "Henosis_0.1.0-alpha.6_amd64.deb"), "deb");
  await writeFile(
    join(sourceDirectory, "Henosis_0.1.0-alpha.6_amd64.AppImage"),
    "appimage",
  );

  /** Paths emitted by the release collector. */
  const collectedFiles = await collectReleaseAssets({
    sourceDirectory: join(testDirectory, "bundle"),
    outputDirectory,
    version: "0.1.0-alpha.6",
    target: "linux-x86_64",
    expectedSuffixes: ".deb,.AppImage",
  });

  assert.deepEqual(
    collectedFiles.map((file) => file.slice(outputDirectory.length + 1)),
    [
      "henosis-desktop-0.1.0-alpha.6-linux-x86_64.deb",
      "henosis-desktop-0.1.0-alpha.6-linux-x86_64.AppImage",
    ],
  );
  assert.equal(await readFile(collectedFiles[0], "utf8"), "deb");
  assert.equal(await readFile(collectedFiles[1], "utf8"), "appimage");
});

test("rejects ambiguous bundle output", async (context) => {
  /** Isolated directory removed after this test. */
  const testDirectory = await createTestDirectory();
  context.after(() => rm(testDirectory, { recursive: true, force: true }));
  /** Tauri bundle directory containing duplicate installer suffixes. */
  const sourceDirectory = join(testDirectory, "bundle");

  await mkdir(sourceDirectory, { recursive: true });
  await writeFile(join(sourceDirectory, "Henosis-a.dmg"), "first");
  await writeFile(join(sourceDirectory, "Henosis-b.dmg"), "second");

  await assert.rejects(
    collectReleaseAssets({
      sourceDirectory,
      outputDirectory: join(testDirectory, "release"),
      version: "0.1.0-alpha.6",
      target: "macos-aarch64",
      expectedSuffixes: ".dmg",
    }),
    /expected exactly one \.dmg bundle/,
  );
});
