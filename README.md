# Henosis

Henosis is a governed operating system for persistent autonomous agents. One principal carries an
agent's identity, trust, credentials, tasks, memory, and quality history across model calls,
process restarts, and work sessions.

Each agent has one principal identity. Soma records its presence, Pistis evaluates its trust and
capabilities, `phylaxd` brokers its credentials, Kleos holds its memory, and Axon carries its
actions to the rest of the system. The agent does not need a separate identity for each subsystem.

Every action crosses an ordered authority chain before execution. Henosis records the decision and
projects the outcome into task state, presence, narration, workflows, and quality measurements.
This gives operators a durable answer to four questions: who acted, why policy allowed it, which
credential boundary it crossed, and what changed afterward.

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

The dispatcher applies the gate chain in that order. The fifth slot uses the legacy in-process
credential-policy implementation while the `phylaxd` integration is completed. The server rejects
a different order and refuses to start without the required Plutus and credential-policy
authorities. An allowed action executes through Hermes or an internal handler, then publishes typed
lifecycle events through Axon.

This structure gives one agent a continuous record of who it is, what it has done, which
capabilities it has earned, and which work remains after a restart.

## Product focus

Agent runtimes often compete on model, tool, and chat-channel counts. Henosis concentrates on the
control plane required after agents receive durable access and unattended work:

| Requirement | Henosis mechanism |
|---|---|
| Stable identity | One principal shared across the runtime |
| Action authorization | Pistis trust, Plutus RBAC and quota, Eidolon policy, human approval, and credential policy |
| Credential containment | `cred` through `phylaxd`, with policy checks before use |
| Durable coordination | Chiasm tasks, Soma presence, Loom workflows, and Axon lifecycle events |
| Operational history | Broca action records and restart-safe service stores |
| Quality feedback | Thymus evaluations and drift signals feed later decisions |
| Model independence | Runtime-selected providers and OpenAI-compatible endpoints |

Henosis aims to make long-running agents governable. Provider and adapter breadth remains useful,
but each new integration must enter through the same identity, policy, credential, event, and
quality boundaries.

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

The installer supports Linux and installs `syntheos-server` as a systemd user service. Start a
loopback-only development runtime without PostgreSQL:

```sh
git clone https://github.com/Syntheos-Systems/henosis.git
cd henosis
./install.sh --local
```

The local path runs the real Plutus gate with one generated owner identity, Free-tier daily quotas,
and a token-bucket rate limit. Its policy counters reset with the process. Identity, tasks,
presence, workflows, action records, quality data, and credentials remain in the private SQLite
files under `~/.local/share/henosis/`.

Check the installed service:

```sh
systemctl --user status henosis
curl http://127.0.0.1:8088/health
```

A production or multi-tenant source install needs:

- Git, Rust 1.88 or newer, Cargo, OpenSSL, and ripgrep
- Rust 1.94 or newer when building the optional `cognition` feature
- A reachable PostgreSQL database for the Plutus authority
- `curl` or `wget` for the startup health check
- A systemd user manager, unless you pass `--no-service`

Run the installer without `--local` and provide PostgreSQL when prompted:

```sh
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

The integrated server binds to `127.0.0.1:8088` unless `SYNTHEOS_ADDR` selects another IP socket
address. Its kernel APIs currently use caller-asserted tenant and principal IDs, so the server
rejects non-loopback binds by default. `SYNTHEOS_ALLOW_INSECURE_REMOTE=1` permits a deliberate
development bind only when an authenticated private boundary protects the server. Its base routes
include:

- `/health`, `/version`, `/enroll`, and `/dispatch`
- Chiasm task, Soma agent, Broca action, Loom workflow, and Thymus quality APIs
- Optional `/cognition/*` routes in a build with `--features cognition`
- Optional operator authentication, dashboard, and WebSocket routes when an operator JWT secret is configured
- Optional Stripe entitlement webhook when its signing secret is configured

The standalone Rift server supplies room, message, membership, upload, bridge-control, and
WebSocket APIs. It defaults to `127.0.0.1:3200`, requires distinct `JWT_SECRET` and
`RIFT_BRIDGE_SECRET` values of at least 32 bytes, and limits browser access to local origins unless
`RIFT_CORS_ORIGINS` supplies an explicit comma-separated allowlist. The standalone memory gateway
also rejects non-loopback binds unless `SYNTHEOS_GATEWAY_ALLOW_INSECURE_REMOTE=1` is set behind a
trusted authenticated boundary. Synapse ships CLI and TUI clients for provider-backed agent
sessions.

## Providers and product boundaries

Henosis makes model and storage integrations replaceable where substitution serves users. The
authority chain remains part of the product security model.

| Area | Choices or interface | Status |
|------|----------------------|--------|
| Synapse models | Anthropic, custom OpenAI-compatible proxies, Ollama, Azure OpenAI, OpenCode Zen, OpenAI Codex, Palantir Foundry, and Claude Max | Selected at runtime |
| Hephaestus models | Anthropic or an OpenAI-compatible endpoint such as OpenAI, Ollama, Azure, or OpenRouter | Selected through environment configuration |
| Rift embeddings | OpenAI-compatible HTTP, shared in-process Kleos bge-m3, or token overlap | Runtime and feature configuration |
| Rift memory | External Kleos HTTP or in-process Cognition | External by default; embedded with the `cognition` feature |
| Pistis policy | Proprietary decision core with a `RoomStateSource` input | The decision core is a locked Henosis component; its separate source repository remains private, while room-state providers can implement the public trait |
| Plutus policy data | `PolicyBackend` trait | Integrated server uses PostgreSQL |
| Human approval | `Approver` trait | Integrated server uses the Rift approval registry |
| Tool adapters | Hermes tool trait and registry | Adding a provider requires a Rust adapter and registry entry |
| Credentials | `cred`, brokered by `phylaxd` | Canonical external credential path; Henosis integration is incomplete |

Kleos is the native memory and cognition system. PostgreSQL is the production Plutus backend.
The ordered policy chain is intentional Henosis architecture, while traits isolate its data
sources and callers.

### Credential integration gap

`phylaxd` is the credential broker behind `cred`. Hermes uses its compatibility API for OAuth
tokens and webhook secrets needed by Gmail, Google Calendar, Google Drive, GitHub, Linear, Notion,
and similar adapters. Henosis-owned external-broker clients use `phylaxd` terminology; the
compatibility wire endpoints remain an implementation detail of the broker.

The repository also contains a legacy in-process credential-policy crate wired as the fifth gate
and credential store. That crate came from a discontinued experiment and is not the intended
credential architecture. Henosis must replace that path with `phylaxd` before the credential
integration is considered complete.

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
It requires Rust 1.94 or newer, matching the pinned Kleos upstream workspace.
The stub scan requires ripgrep. Run the repository checks with:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
./tests/install.sh
./scripts/stub-scan.sh
```

## Active-development limits

- Local policy mode supports one generated owner on loopback. Its quota and rate-limit counters
  reset on restart, it does not support Stripe billing or operator-account bootstrap, and it is not
  a production or multi-tenant backend.
- Kernel APIs use caller-asserted tenant and principal IDs. Keep the integrated server on loopback;
  non-loopback binds require an explicit insecure-development override and an authenticated private
  boundary.
- Rift has no in-process authentication rate limiter. Keep it on loopback or put a rate-limiting
  proxy in front of it.
- Rift attachment URLs are opaque bearer capabilities, not authenticated download routes. Do not
  use them for sensitive files. Attachment file reclamation is incomplete.
- Logout and password changes revoke Rift refresh tokens. Already issued access tokens remain valid
  for their 24-hour lifetime. Rotate `JWT_SECRET` to invalidate them after an account or signing-key
  compromise.
- The integrated Pistis gate uses an empty in-memory room-state source. Requests that require room capabilities fail closed until live room state is connected.
- Hephaestus has a placeholder production Plutus token provider for tenant-scoped Anthropic authentication. Development credentials and OpenAI-compatible providers use separate paths.
- Credential handling is split between the canonical `phylaxd` broker and the legacy in-process
  credential-policy path described above.
- The default Rift bridge expects a reachable Kleos HTTP service. An embedded memory path requires the `cognition` feature.
- The source installer configures `syntheos-server`; it does not provision PostgreSQL or install every workspace binary.
- Public APIs and persistence formats have no stable compatibility guarantee yet.

The repository keeps test-only allow gates behind the non-default `stubs` feature. Production
startup uses the five authority implementations and fails when required authority configuration is
missing.

## Security reports

Send vulnerabilities through [GitHub Private Vulnerability Reporting](https://github.com/Syntheos-Systems/henosis/security/advisories/new)
or `security@syntheos.dev`. Send non-security bugs to `support@syntheos.dev` or open a GitHub
issue that contains no credentials, private data, or exploit details. Read [SECURITY.md](SECURITY.md)
before sending sensitive material.

## License

Henosis is source-available under the [Elastic License 2.0](LICENSE). Elastic License 2.0 is not an
OSI-approved open-source license and restricts offering the software as a hosted or managed
service.
