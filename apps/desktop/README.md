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

For the terminal-free operator path, supported installer names, and current
platform trust warnings, use the public [desktop installation
guide](../../docs/desktop-install.md).

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

## Room dashboard

Conversation remains the primary room surface. Room controls open beside it on
wide screens and as a keyboard-contained sheet on narrower screens. The
dashboard has three tabs:

- **Agents** shows the ordered room roster. Execution harness and model are
  separate controls populated from the deployment capability catalog, including
  availability and supported non-secret settings.
- **People** groups human members and persistent agent identities by ownership.
  An operator may configure an identity they own. A room manager may explicitly
  claim a visible unowned import, but seeing an identity in a roster does not
  grant ownership.
- **Room** shows read-only server and bridge context. Operators with
  `manageServer` permission may pause or resume the bridge; room rename and
  policy mutation are outside this dashboard.

Agent edits stay local until **Apply roster** sends one normalized whole-roster
desired state with the current expected revision. **Discard changes** restores
the last server snapshot. A concurrent revision never silently overwrites the
local or remote version: the dashboard preserves the draft, loads current
server truth, and offers field-level review or an explicit discard.

After an accepted write, the desired revision is distinct from runtime state.
The dashboard reports pending, active, or failed activation and identifies the
last good revision when activation fails. Status checks use capped intervals
and stop after 60 seconds; a timeout leaves the desired state pending and offers
manual Refresh. Retry reconciles the same desired revision instead of creating
another roster revision. Lost Rift authentication preserves safe visible state
and routes recovery through an explicit Reconnect action.

The React layer receives capability metadata, ownership identifiers, stable
error codes, credential readiness, and at most an opaque credential-binding
UUID. Rift tokens, Phylax credential values, and binding locator metadata remain
inside native or server boundaries. Direct messages and human invitation
mutations are intentionally absent from the current interface.

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
