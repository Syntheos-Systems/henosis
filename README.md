# Henosis

An autonomous agent operating system. One runtime, one authority model, one
coordination plane, one tool/action path, one memory system, one operator
surface, one deployment story.

Syntheos-OS unifies the independent Syntheos-Systems services -- Kleos, Phylax,
Pistis, Rift, Synapse, Hephaestus, Hermes, and future Athena/Plutus -- into a
single Cargo workspace that boots as one process. Services keep their domain
authority and behavior; the OS provides shared contracts, canonical identity,
typed events, and a unified action dispatcher so they compose instead of
collide.

## Architecture

Four internal layers:

```
 External Surfaces     MCP, CLI, HTTP APIs, Rift Web, Athena Web, compatibility shims
        |
 Runtime Services      Rift (chat), Synapse (cognition), Hephaestus (execution), Hermes (tools)
        |
 Kernel Services       Engram (memory), Axon (events), Chiasm (coordination), Soma (presence),
                        Broca (narration), Loom (workflows), Thymus (evaluation), Agent-Forge
                        (reasoning protocol), Eidolon (supervision), Phylax (secrets), Pistis
                        (identity/trust), Plutus (tenancy/billing)
        |
 Storage Engines        Postgres, SQLite (Engram), service-owned schemas
```

Services share canonical contracts -- principal identity, tenant, event
envelope, task reference, tool invocation, credential handle -- but own their
operational state. Internal communication is in-process Rust APIs and typed
Axon event subscriptions, not HTTP between old daemons.

## What's Inside

| Subsystem | Role |
|-----------|------|
| **Kleos/Engram** | Long-term memory: ingestion, dedup, embeddings, FSRS spaced repetition, search, context assembly, recall scheduling |
| **Axon** | Pub/sub event fabric: channels, fanout, retention, subscriptions, replay cursors, typed envelopes |
| **Chiasm** | Task coordination: status, assignment, claims, dependencies, heartbeats, queues, operator visibility |
| **Soma** | Agent presence and registry: online/idle/offline, heartbeat, capability advertisement, health |
| **Broca** | Narration service: consumes events and service metadata, produces human-readable action feeds |
| **Loom** | Workflow orchestration: DAG execution, scheduling, retries, durable step state |
| **Thymus** | Evaluation and quality: rubrics, metrics, drift detection, score history |
| **Agent-Forge** | Structured reasoning and verification protocol: spec, hypothesis, approach, challenge, evidence |
| **Eidolon** | Supervisor/evaluator: input/output gating, content checks, policy enforcement, drift detection |
| **Phylax** | Secret authority and credential vault: Vaultwarden-compatible API, agent-native grants, leases, approval-gated reads, ECDH/PIV bootstrap, audit |
| **Pistis** | Identity, trust, and capabilities: signed event log, admission, grants/revocations, persona scopes, trust computation, cryptographic verification |
| **Rift** | Conversation platform: rooms, messages, memberships, realtime WebSocket, human/agent bridge |
| **Synapse** | Agent cognition runtime: LLM provider abstraction, tool loop, sessions, hooks, scheduled runs |
| **Hephaestus** | Stateful executor: process lifecycle, checkpoints, cancellation, resume, execution logs |
| **Hermes** | External tool gateway: Gmail/Drive/Calendar/GitHub/Slack adapters, OAuth refresh, rate limits, MCP bridge |
| **Plutus** | Tenancy, billing, RBAC, quota enforcement (future) |
| **Athena** | Operator workbench UI and SDK (future) |

## Unified Action Dispatch

Every action an autonomous agent takes flows through one dispatcher:

```
request context { tenant, principal, persona, session, room?, task?, workflow? }
  |
  v
syntheos action dispatcher
  |-- resolve tool/action from skills + adapter registries
  |-- PistisGate:  capability/persona authorization
  |-- PlutusGate:  tenant role, quota, billing, rate limits
  |-- EidolonGate: content/prompt-injection/policy checks (input)
  |-- HumanGate:   approval for destructive or high-risk operations
  |-- PhylaxGate:  credential authorization, approval, lease, resolve mode
  |-- execute:     Synapse in-process tool | Hermes external adapter | OS internal action
  |-- EidolonGate: output filtering/redaction/drift checks
  |-- Axon:        publish redacted tool/action event
  |-- Chiasm:      update task progress
  |-- Broca:       narrate for humans
  |-- Thymus:      evaluate if scored
  |-- Engram:      promote if worth remembering
  |
  v
result
```

The agent runtime never calls Hermes, Phylax, shell, or external APIs
directly. It calls the dispatcher.

## Principal Identity

One canonical principal ID per human, agent, service account, or integration.
Each service maintains its own projection:

- **Soma** -- presence, heartbeat, availability, capabilities
- **Rift** -- display name, avatar, room role, chat preferences
- **Pistis** -- signed admission, persona grants, capability grants, trust evidence
- **Phylax** -- secret scopes, leases, token/grant binding, approval policy
- **Identity keys** -- external API signing, MCP auth, PIV/software keys
- **Plutus** -- tenant roles, entitlements, quotas, billing subject

They share the principal key and contract. They do not erase each other.

## Memory Model

Engram stores what the OS should remember. Operational data is not
automatically memory -- promotion is explicit and typed:

| Type | Source | Meaning |
|------|--------|---------|
| `chat_insight` | Rift | A message or thread became durable knowledge |
| `task_outcome` | Chiasm/Hephaestus/Agent-Forge | A completed task result with evidence |
| `credential_decision` | Phylax | A credential access decision worth retaining |
| `pistis_snapshot` | Pistis | A materialized trust/capability state snapshot |
| `quality_signal` | Thymus | An eval or drift result worth future retrieval |
| `workflow_checkpoint` | Loom/Hephaestus | A checkpoint worth resuming or teaching from |

Rift messages stay in Rift. Axon events stay in Axon. Phylax audit stays in
Phylax. Engram stores what the OS should remember.

## Autonomy Loop

The full cognitive-behavioral loop:

```
sense -> deliberate -> plan -> authorize -> act -> observe -> remember
```

Loom DAGs wire Chiasm task surfaces to Hephaestus execution. Agent-Forge
evidence is part of task/run completion. Human interruption and approval flow
through Rift, Athena, and MCP.

## Build and Run

Single Cargo workspace. One binary, feature-gated:

```sh
cargo build --release -p syntheos-server          # full OS
cargo build --release -p syntheos-server --no-default-features --features kernel,rift  # just kernel + chat
```

Default ports:

| Port | Surface |
|------|---------|
| 4200 | Core API (memory, Axon, Chiasm, Soma, Broca, Loom, Thymus, admin) |
| 4510 | Memory gateway (FrameShift compatibility shim) |
| 3200 | Rift (REST + WebSocket chat) |
| 4700 | Hephaestus (task submission/control) |
| 4800 | Hermes (external tool invocation) |
| 8080 | Phylax (Vaultwarden-compatible + agent-native API) |
| 9091 | MCP (external agent integration) |
| 5000 | Plutus (tenancy + billing, future) |
| 4900 | Athena (operator workbench, future) |

## Current Status

Early workspace. The memory gateway (`syntheos-memory-gateway`) is the first
crate, translating between FrameShift's wire contract and Kleos's memory API
with Ed25519 identity signing. The unified architecture plan covers 11
absorption waves bringing the independent services into the workspace over
approximately 24 weeks.

## License

Elastic License 2.0 (ELv2). See `LICENSE`.
