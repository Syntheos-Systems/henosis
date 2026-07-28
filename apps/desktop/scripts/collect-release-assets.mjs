#!/usr/bin/env node
/**
 * Collect Tauri bundle outputs under stable names for the unified Henosis
 * release publisher.
 */

import { constants as fileConstants } from "node:fs";
import { copyFile, mkdir, readdir } from "node:fs/promises";
import { basename, join } from "node:path";
import { pathToFileURL } from "node:url";

/** Accepted release versions shared by server and desktop packages. */
const VERSION_PATTERN = /^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$/;

/** Accepted normalized target labels used in public artifact filenames. */
const TARGET_PATTERN = /^[a-z0-9]+(?:[-_][a-z0-9]+)*$/;

/** Recursively list regular files without following directory symlinks. */
async function listRegularFiles(directory) {
  /** Directory entries at the current traversal depth. */
  const entries = await readdir(directory, { withFileTypes: true });
  /** Regular files collected below the current directory. */
  const files = [];

  for (const entry of entries) {
    /** Absolute path for the current entry. */
    const entryPath = join(directory, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await listRegularFiles(entryPath)));
    } else if (entry.isFile()) {
      files.push(entryPath);
    }
  }

  return files;
}

/** Parse and validate the comma-separated installer suffix contract. */
function parseExpectedSuffixes(value) {
  /** Non-empty suffixes requested by the release matrix. */
  const suffixes = value.split(",").filter(Boolean);
  if (
    suffixes.length === 0 ||
    suffixes.some((suffix) => !/^\.[A-Za-z0-9]+$/.test(suffix)) ||
    new Set(suffixes).size !== suffixes.length
  ) {
    throw new Error("expected suffixes must be unique values such as .deb,.AppImage");
  }
  return suffixes;
}

/** Collect exactly one Tauri bundle for each required installer suffix. */
export async function collectReleaseAssets({
  sourceDirectory,
  outputDirectory,
  version,
  target,
  expectedSuffixes,
}) {
  if (!VERSION_PATTERN.test(version)) {
    throw new Error(`invalid release version: ${version}`);
  }
  if (!TARGET_PATTERN.test(target)) {
    throw new Error(`invalid release target: ${target}`);
  }

  /** Validated suffixes expected for this runner. */
  const suffixes = parseExpectedSuffixes(expectedSuffixes);
  /** All regular bundle files produced by Tauri. */
  const bundleFiles = await listRegularFiles(sourceDirectory);
  /** Stable public paths copied for artifact upload. */
  const collectedFiles = [];

  await mkdir(outputDirectory, { recursive: true });

  for (const suffix of suffixes) {
    /** Bundle files matching the current required suffix. */
    const matches = bundleFiles.filter((file) => basename(file).endsWith(suffix));
    if (matches.length !== 1) {
      throw new Error(
        `expected exactly one ${suffix} bundle for ${target}, found ${matches.length}`,
      );
    }

    /** Stable destination for the current installer. */
    const destination = join(
      outputDirectory,
      `henosis-desktop-${version}-${target}${suffix}`,
    );
    await copyFile(matches[0], destination, fileConstants.COPYFILE_EXCL);
    collectedFiles.push(destination);
  }

  return collectedFiles;
}

/** Execute the collector as a command-line release utility. */
async function main() {
  /** Positional command-line arguments after the Node executable and script. */
  const [sourceDirectory, outputDirectory, version, target, expectedSuffixes] =
    process.argv.slice(2);
  if (
    !sourceDirectory ||
    !outputDirectory ||
    !version ||
    !target ||
    !expectedSuffixes
  ) {
    throw new Error(
      "usage: collect-release-assets.mjs SOURCE OUTPUT VERSION TARGET SUFFIXES",
    );
  }

  /** Installer paths prepared for the artifact uploader. */
  const collectedFiles = await collectReleaseAssets({
    sourceDirectory,
    outputDirectory,
    version,
    target,
    expectedSuffixes,
  });
  process.stdout.write(`${collectedFiles.join("\n")}\n`);
}

/** Canonical URL of the current command-line script, when one was provided. */
const invokedScriptUrl = process.argv[1]
  ? pathToFileURL(process.argv[1]).href
  : undefined;
if (invokedScriptUrl === import.meta.url) {
  main().catch((error) => {
    process.stderr.write(`collect-release-assets: ${error.message}\n`);
    process.exitCode = 1;
  });
}
