# Henosis

Henosis is an agent runtime built around one identity model, one authorization path, and one event-driven coordination plane. The repository is under active development. APIs, storage schemas, and deployment requirements may change before the first stable release.

The Cargo workspace combines the Syntheos kernel authorities with agent execution, chat, memory, tools, workflows, and operator services. Each subsystem owns its state and communicates through typed Rust contracts and Axon events.

## Architecture

```text
External surfaces
  HTTP APIs, MCP, CLI, TUI, Rift chat, operator API
        |
Runtime services
  Synapse, Hephaestus, Hermes, Rift
        |
Syntheos kernel
  Identity, Axon, Chiasm, Soma, Broca, Loom, Thymus
  Pistis, Plutus, Eidolon, Human approval, Phylax
        |
Storage and cognition
  SQLite, Postgres, Kleos, optional in-process Cognition
```

The `syntheos-server` binary composes the kernel and its canonical action dispatcher. Rift, its agent bridge, Synapse clients, and compatibility gateways also ship as workspace binaries.

## Action authorization

Henosis sends tool and system actions through one ordered gate chain:

```text
invocation
  -> Pistis capability and trust policy
  -> Plutus tenant, role, quota, and rate policy
  -> Eidolon input policy
  -> human approval when required
  -> Phylax credential policy
  -> Hermes, Phylax, or internal execution
  -> Eidolon output filtering
  -> Axon event publication and service projections
```

The server rejects a gate chain with a different order. Production startup also requires the Plutus and Phylax authorities; it does not replace missing authorities with allow or deny placeholders.

## Subsystems

| Subsystem | Responsibility |
|-----------|----------------|
| Axon | Typed event publication, subscriptions, retention, and replay |
| Chiasm | Task state, claims, dependencies, activity, and queues |
| Soma | Agent presence, health, and capability advertisement |
| Broca | Human-readable action narration |
| Loom | Durable workflow DAGs and step execution |
| Thymus | Evaluations, quality history, and drift signals |
| Pistis | Capability grants, trust, admission, and room authority |
| Plutus | Organizations, roles, quotas, rate limits, and billing entitlements |
| Eidolon | Input policy, supervision, output filtering, and redaction |
| Phylax | Encrypted credentials, grants, leases, and use-without-holding execution |
| Rift | Rooms, messages, membership, WebSockets, and human approval |
| Synapse | Provider-neutral chat requests, tool loops, sessions, and clients |
| Hephaestus | Stateful agent execution, checkpoints, cancellation, and resume |
| Hermes | External SaaS adapters, OAuth refresh, rate limits, circuits, and MCP |
| Cognition | Optional in-process access to the vendored Kleos cognitive core |
| Agent-Forge | Task specifications, hypotheses, review gates, and verification records |

## Provider and backend boundaries

Henosis separates configurable providers from product authorities.

| Area | Available substitutions | Boundary |
|------|-------------------------|----------|
| Synapse LLM | Anthropic, OpenAI-compatible endpoints, Ollama, Azure OpenAI, OpenCode Zen, OpenAI Codex, Palantir Foundry, Claude Max | Runtime provider configuration |
| Hephaestus LLM | Anthropic or an OpenAI-compatible endpoint such as OpenAI, Ollama, Azure, or OpenRouter | Runtime environment configuration |
| Rift embeddings | OpenAI-compatible HTTP endpoint, shared in-process Kleos bge-m3, or token-overlap mode | Runtime configuration plus the optional `cognition` feature |
| Rift memory | External Kleos HTTP service or in-process Cognition | Compile-time `cognition` feature and bridge configuration |
| Hermes tools | Tool implementations register through a common trait and registry | New adapters require Rust code; bundled OAuth adapters resolve credentials through `credd` |
| Pistis room state | `RoomStateSource` trait | The server still uses an in-memory source pending live room materialization |
| Plutus policy reads | `PolicyBackend` trait | The production server uses Postgres |
| Human approval | `Approver` trait | The production server uses the Rift approval registry |

The Syntheos authority roles and their gate order define the product security model. Replacing a data source behind an authority does not remove that authority from the dispatcher.

Kleos remains the native memory and cognition system. The Rift bridge uses the Kleos HTTP API in its default build. Enabling `cognition` embeds the vendored `kleos-lib` facade and its optional local bge-m3 provider in the process.

## Install

The Linux installer turns a checkout into a persistent user service. It validates Postgres,
builds the integrated server, generates the required UUIDv8 authority identities and Phylax
master key, writes an owner-only environment file, installs a hardened systemd user unit, starts
the service, and verifies the real health endpoint:

```sh
git clone https://github.com/Syntheos-Systems/henosis.git
cd henosis
./install.sh
```

The fresh-install prompt reads the Postgres URL without echo, keeping database credentials out of
shell history. Automation can set `SYNTHEOS_PLUTUS_DB` or pass `--postgres-url` directly.

Postgres is an explicit external boundary because Plutus is a required production authority. The
installer does not silently start a privileged database container or invent database credentials.
It currently builds `syntheos-server` from the checkout, so a Rust toolchain and OpenSSL are
required. A prebuilt executable can bypass the build with `--binary /path/to/syntheos-server`.

Configuration is stored at `~/.config/henosis/henosis.env`, persistent SQLite state at
`~/.local/share/henosis/`, and the binary at `~/.local/bin/syntheos-server`. Re-running the same
command, or simply `./install.sh` after the first install, updates the binary and service definition
while preserving the environment file byte for byte, so authority identities and encryption keys
do not rotate. Prior binaries and changed service definitions receive uniquely named timestamped
backups.

Useful service commands:

```sh
systemctl --user status henosis
journalctl --user -u henosis -f
systemctl --user restart henosis
```

For a foreground or non-systemd installation, pass `--no-service`. Run `./install.sh --help` for
path overrides, a custom listen address, service-only installation without startup, and prebuilt
binary support. The installer is Linux-first while Henosis remains in active development.

## Build

Install a Rust toolchain compatible with the workspace lockfile, then run:

```sh
cargo build --workspace
```

Build the integrated server without the local cognitive core:

```sh
cargo build -p syntheos-server
```

Build it with in-process Cognition:

```sh
cargo build -p syntheos-server --features cognition
```

The `cognition` feature compiles the vendored Kleos machine-learning dependencies. Default builds leave that stack out.

## Running the server

`syntheos-server` binds to `127.0.0.1:8088` unless `SYNTHEOS_ADDR` overrides it. A production-shaped boot requires at least:

- `SYNTHEOS_PLUTUS_DB`, a Postgres connection URL
- `SYNTHEOS_PLUTUS_OPERATOR_TENANT`, a tenant UUID used for first-boot organization setup
- `SYNTHEOS_PLUTUS_OPERATOR_PRINCIPAL`, a principal UUID used for first-boot organization setup
- `SYNTHEOS_PHYLAX_KEY`, a 64-character hexadecimal master key

SQLite-backed services write under `data/` unless their `SYNTHEOS_*_DB` variables select other paths. Optional surfaces use their own environment variables, including the operator JWT, Stripe webhook, Eidolon supervisor, Hephaestus provider, and Cognition database settings.

Do not use development keys in a deployed instance. Keep secrets outside the repository and inject them through the deployment environment or credential service.

## Development status

The core workspace builds and contains working implementations for the dispatcher, kernel stores, authorization gates, Hermes execution, task and narration projections, agent execution, Rift, Synapse, and optional Cognition.

The following work remains open:

- Hephaestus production tenant-scoped Anthropic authentication has a placeholder Plutus token provider. Development mode can use local Claude credentials, and OpenAI-compatible providers use their configured key path.
- The integrated server constructs an empty in-memory Pistis room-state source. Capability-bearing requests fail closed until a live source supplies signed room state.
- The default Rift bridge memory path requires a reachable Kleos HTTP service. In-process memory requires a build with `--features cognition`.
- Hermes bundles adapters for several SaaS providers, but adding a different service requires a Rust adapter and registry entry.
- Public APIs and persistence formats have not reached a stable compatibility guarantee.

See [`scripts/known-incomplete.md`](scripts/known-incomplete.md) for the maintained wiring ledger and dependency advisory status.

## Security and issue reports

Open a GitHub issue for reproducible bugs that do not expose sensitive data. Send security reports through a private maintainer channel until the project publishes a security policy and disclosure address.

## License

Henosis uses the Elastic License 2.0. See [`LICENSE`](LICENSE).
