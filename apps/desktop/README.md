# Henosis desktop

Henosis desktop is the graphical operating environment for Henosis. It opens
to the Rift room directory, pins the most recently active room, and keeps
Athena as one internal workbench rather than treating it as the application
shell.

The desktop application lives in the Henosis monorepo so GUI and backend
changes can share commits, review, API contracts, and release tags. Its Tauri
crate is intentionally a nested Cargo workspace. This keeps desktop platform
dependencies out of the headless server workspace while preserving one source
of truth.

## Local development

Install the pinned JavaScript dependencies:

```sh
pnpm install --frozen-lockfile
```

Run the browser development surface with fixture rooms:

```sh
pnpm dev
```

Run the native desktop application against a real Rift endpoint:

```sh
pnpm tauri dev
```

The browser surface is explicitly fixture-backed. A production build uses the
Tauri adapter, retains Rift tokens in the native process, and saves only
sanitized connection and room-cache data.

## Verification

```sh
pnpm test
pnpm test:release
pnpm build
cargo +1.88.0 test --locked --manifest-path src-tauri/Cargo.toml
```

## Releases

Desktop versions match `crates/syntheos-server/Cargo.toml`. A signed Henosis
release tag builds Linux, macOS, and Windows installers. The protected
publication job validates the complete native and desktop artifact set,
generates checksums, and attaches the same provenance attestations used by the
headless release.

Current desktop artifacts are installable but not store-trusted:

- macOS applications receive an ad hoc signature, not Apple notarization.
- Windows installers are not Authenticode-signed.
- In-app updater artifacts are not generated until a Tauri updater signing key
  and public key are provisioned through the release environment.

Those trust features require credentials that are intentionally not
hardcoded or synthesized by the build.
