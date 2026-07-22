# Henosis

Henosis is an operating system for persistent autonomous agents. It keeps identity, memory,
trust, coordination, execution, policy, and credentials in one runtime so an agent can retain
continuity across model calls, process restarts, and work sessions.

Each agent has one principal identity. Soma records its presence, Pistis evaluates its trust and
capabilities, `phylaxd` brokers its credentials, Kleos holds its memory, and Axon carries its
actions to the rest of the system. The agent does not need a separate identity for each subsystem.

> **Status:** Henosis is under active development. The integrated server runs, the source installer
> produces a persistent user service, and the core authorities fail closed. APIs, database schemas,
> configuration, and deployment requirements may change before the first stable release.

## Operating model

Henosis combines the agent lifecycle in one Rust workspace:

```text
Human, agent, CLI, TUI, HTTP, MCP, or Rift
                    |
             Synapse / Hephaestus
          model calls, tools, checkpoints
                    |
          canonical action dispatcher
                    |
    Pistis -> Plutus -> Eidolon -> Human -> credential policy
                    |
         Hermes or internal execution
                    |
      Axon events update Chiasm, Soma, Broca,
      Loom, Thymus, Rift, and durable storage
                    |
        Kleos memory and Cognition services
```

The dispatcher applies the gate chain in that order. The current source calls the last internal
slot `phylax`, a stale name retained by the in-process implementation. The server rejects a
different order and refuses to start without the required Plutus and credential-policy
authorities. An allowed action executes through Hermes or an internal handler, then publishes
typed lifecycle events through Axon.

This structure gives one agent a continuous record of who it is, what it has done, which
capabilities it has earned, and which work remains after a restart.

## What is in the workspace

| Area | Components | Responsibility |
|------|------------|----------------|
| Integrated runtime | `syntheos-server`, contracts, identity, dispatch | Boots the authorities, kernel services, execution path, HTTP API, and optional operator surface |
| Coordination | Axon, Chiasm, Soma, Broca, Loom, Thymus | Events, tasks, presence, narration, workflows, evaluations, and drift signals |
| Authorities | Pistis, Plutus, Eidolon, Human approval, credential policy | Capability policy, tenant policy, input and output policy, escalation, and credential use |
| Agent execution | Synapse, Hephaestus, Hermes | Model providers, sessions, checkpoints, tool loops, SaaS adapters, rate limits, and audit events |
| Agent space | Rift server and bridge | Rooms, messages, WebSockets, agent participation, and approval requests |
| Memory | Kleos bridge and optional Cognition facade | Persistent memory, context, handoffs, graph, personality, skills, and intelligence services |
| Development gates | Agent-Forge | Task specifications, hypotheses, review checks, and verification records |

The default `syntheos-server` build excludes the vendored Kleos machine-learning stack. The
`cognition` feature embeds that stack and exposes the Cognition routes in the same process.

## Quick start

The installer supports Linux and installs `syntheos-server` as a systemd user service. A source
install needs:

- Git, Rust 1.88 or newer, Cargo, OpenSSL, and ripgrep
- A reachable PostgreSQL database for the Plutus authority
- `curl` or `wget` for the startup health check
- A systemd user manager, unless you pass `--no-service`

Clone the repository and run the installer:

```sh
git clone https://github.com/Syntheos-Systems/henosis.git
cd henosis
./install.sh
```

On a new installation, the script requests the PostgreSQL URL without echoing it. It generates the
operator tenant, operator principal, and a key for the current in-process credential path; creates
private SQLite storage; builds the locked release binary; installs a hardened user service; and
checks `/health` after startup.
If `psql` is installed, the script tests the database before the build. The server performs its own
database check at boot in either case.

Automation can supply the database URL through the environment:

```sh
SYNTHEOS_PLUTUS_DB="$DATABASE_URL" ./install.sh
```

The installer stores files in these locations by default:

| Path | Contents |
|------|----------|
| `~/.config/henosis/henosis.env` | Owner-only runtime configuration and generated authority secrets |
| `~/.local/share/henosis/` | Persistent SQLite databases |
| `~/.local/bin/syntheos-server` | Installed server binary |
| `~/.config/systemd/user/henosis.service` | Hardened systemd user unit |

Run the same command after pulling changes to update the binary and service definition. The
installer preserves the environment file byte for byte, so upgrades do not rotate identities or
keys.

Useful commands:

```sh
systemctl --user status henosis
journalctl --user -u henosis -f
systemctl --user restart henosis
curl http://127.0.0.1:8088/health
```

Use `./install.sh --no-service` for a foreground or non-systemd installation. Pass `--binary` to
install a prebuilt `syntheos-server` without compiling the workspace. Run `./install.sh --help` for
all path and bind-address options.

The installer configures the integrated server. Rift, Synapse CLI/TUI, and standalone compatibility
binaries have separate configuration and launch paths.

## API surfaces

The integrated server binds to `127.0.0.1:8088` unless `SYNTHEOS_ADDR` selects another address. Its
base routes include:

- `/health`, `/version`, `/enroll`, and `/dispatch`
- Chiasm task, Soma agent, Broca action, Loom workflow, and Thymus quality APIs
- Optional `/cognition/*` routes in a build with `--features cognition`
- Optional operator authentication, dashboard, and WebSocket routes when an operator JWT secret is configured
- Optional Stripe entitlement webhook when its signing secret is configured

The standalone Rift server supplies room, message, membership, upload, bridge-control, and
WebSocket APIs. Synapse ships CLI and TUI clients for provider-backed agent sessions.

## Providers and product boundaries

Henosis makes model and storage integrations replaceable where substitution serves users. The
authority chain remains part of the product security model.

| Area | Choices or interface | Status |
|------|----------------------|--------|
| Synapse models | Anthropic, custom OpenAI-compatible proxies, Ollama, Azure OpenAI, OpenCode Zen, OpenAI Codex, Palantir Foundry, and Claude Max | Selected at runtime |
| Hephaestus models | Anthropic or an OpenAI-compatible endpoint such as OpenAI, Ollama, Azure, or OpenRouter | Selected through environment configuration |
| Rift embeddings | OpenAI-compatible HTTP, shared in-process Kleos bge-m3, or token overlap | Runtime and feature configuration |
| Rift memory | External Kleos HTTP or in-process Cognition | External by default; embedded with the `cognition` feature |
| Pistis room state | `RoomStateSource` trait | Integrated server uses an empty in-memory source pending live room materialization |
| Plutus policy data | `PolicyBackend` trait | Integrated server uses PostgreSQL |
| Human approval | `Approver` trait | Integrated server uses the Rift approval registry |
| Tool adapters | Hermes tool trait and registry | Adding a provider requires a Rust adapter and registry entry |
| Credentials | `cred`, brokered by `phylaxd` | Canonical external credential path; Henosis integration is incomplete |

Kleos is the native memory and cognition system. PostgreSQL is the production Plutus backend.
The ordered policy chain is intentional Henosis architecture, while traits isolate its data
sources and callers.

### Credential migration gap

`phylaxd` is the credential broker behind `cred`. It superseded `credd` and retains a
credd-compatible API, which is why some client types, environment variables, socket names, and
error messages still contain the old name.

Hermes uses that compatibility API for OAuth tokens and webhook secrets needed by Gmail, Google
Calendar, Google Drive, GitHub, Linear, Notion, and similar adapters. Its client and configuration
still use the legacy `CreddClient`, `CREDD_URL`, and `HERMES_CREDD_TOKEN` names.

The repository also contains an absorbed `henosis-phylax` crate and currently wires it into the
integrated server as the fifth policy gate and an in-process credential store. It came from a
separate credential experiment and is not the intended credential architecture. Before the
credential integration is complete, Henosis must replace that path with `phylaxd` and retire its
`SYNTHEOS_PHYLAX_KEY` and `SYNTHEOS_PHYLAX_DB` configuration.

## Build and test

Build the workspace with the lockfile:

```sh
cargo build --locked --workspace
```

Build the integrated server without the local cognitive core:

```sh
cargo build --locked -p syntheos-server
```

Build it with in-process Cognition:

```sh
cargo build --locked -p syntheos-server --features cognition
```

The Cognition build compiles the vendored Kleos machine-learning and vector-search dependencies.
The stub scan requires ripgrep. Run the repository checks with:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
./tests/install.sh
./scripts/stub-scan.sh
```

## Active-development limits

- The integrated Pistis gate uses an empty in-memory room-state source. Requests that require room capabilities fail closed until live room state is connected.
- Hephaestus has a placeholder production Plutus token provider for tenant-scoped Anthropic authentication. Development credentials and OpenAI-compatible providers use separate paths.
- Credential handling is split between the canonical `phylaxd` broker, legacy credd-named Hermes
  compatibility code, and the stale in-process `henosis-phylax` path described above.
- The default Rift bridge expects a reachable Kleos HTTP service. An embedded memory path requires the `cognition` feature.
- The source installer configures `syntheos-server`; it does not provision PostgreSQL or install every workspace binary.
- Public APIs and persistence formats have no stable compatibility guarantee yet.

The repository keeps test-only allow gates behind the non-default `stubs` feature. Production
startup uses the five authority implementations and fails when required authority configuration is
missing.

## Security reports

Open a GitHub issue for reproducible bugs that contain no credentials, private data, or exploit
details. Use GitHub private vulnerability reporting for sensitive reports after that channel is
enabled for this repository. Do not publish secrets in an issue.

## License

Henosis is source-available under the [Elastic License 2.0](LICENSE). Elastic License 2.0 is not an
OSI-approved open-source license and restricts offering the software as a hosted or managed
service.
