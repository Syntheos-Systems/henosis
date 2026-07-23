# Known-incomplete ledger (Henosis)

Everything not fully wired, in one place. `scripts/stub-scan.sh` reprints this every run so
nothing here can hide. If a row exists, that thing is NOT "done" -- do not claim it is. When you
close a row, delete it in the same commit.

Rows 1-5 are deliberately-deferred build-plan wiring. Rows 6-18 came from the half-wiring audit
(2026-06-18). Rows 11 (eidolon Drift fail-open), 12 (URL query encoding), and 15 (gateway
401-retry panic) were FIXED and removed; Wave 3 closed row 5 (server cognition route + persistent
store) and narrowed row 2; rows 8
(UpdatePresence now persists via `update_user_status`) and 9 (`get_server_members` now SELECTs the
real agent/timestamp columns) were FIXED and removed; row 6 (`cargo_target_dir` now forwarded to
the executor spawn via `ExecutionSandbox.cargo_target_dir`) was FIXED and removed, and row 7 (the
supervisor now retries a partial-work failure once against the same worktree, threading a resume
summary -- failure reason plus the partial commits -- as `prior_context` via `execution::resume`)
was FIXED and removed;
row 13 (`ToolCompleted` now recovers the real tool name by correlating `ToolResult` to `ToolStart`
on the shared tool-call id) and row 18 (`synapse-tui` `/model` now surfaces a notice in the focused
transcript; `synapse-cli` provider-swap already notified) were FIXED and removed; row 10
(`list_dms` now SELECTs the real `u.status`/`u.is_agent`) and row 14 (the ClaudeCode executor now
reports `commit_hash` by diffing git HEAD across the run; synapse-native never commits, documented)
were FIXED and removed; row 17 (`SynapseExecutor::health_check` now validates static config --
model/max_tokens/max_turns -- and the bridge runs a health preflight in `Room::execute_approved`
before spawning, blocking the task when not `Ready`) and row 16 (the executor's `sandbox()` is now
read as runtime policy: the bridge clamps the worktree's `max_runtime_secs` to the executor's
declared ceiling via `execution::preflight::apply_runtime_policy`) were FIXED and removed. Row 2
was RESOLVED as deliberate design (not a half-wire) and removed: the bridge's HTTP-default memory
path is the intended architecture, not an incomplete wire. In-process cognition memory is opt-in
via `--features cognition` (which pulls the vendored kleos-lib ML stack in), exactly mirroring
syntheos-server, so the default build stays ML-free; KLEOS :4200 coexists permanently (project
decision, 2026-06-18), so routing default-build memory to :4200 over HTTP is a chosen tradeoff,
not a TODO.
Both `HttpMemoryBackend` and `CognitionMemoryBackend` are fully wired and selected at compile time.

The final three deferred build-plan rows were closed 2026-06-29:

Row 1 (plutus gate was a fail-closed deny-stub) was FIXED and removed: the new
`henosis-plutus` kernel authority backs a real `PlutusGate` doing org-status -> RBAC
-> hard-quota -> rate-limit checks, fail-closed at every step (no Allow-on-error
path), replacing the deny-stub in the plutus slot. All five gate slots now run real
gates. Storage is Postgres via sqlx (operator decision D1); the gate depends on a `PolicyBackend`
trait so its fail-closed matrix is unit-tested with no live DB. Billing (Stripe/X402)
is deliberately NOT part of this -- a separate later effort, not needed for the slot
to be real.

Row 3 (cognition facade was a PARTIAL surface) was FIXED and removed: the facade now
covers all the named surfaces -- memory, context, scratchpad, handoffs, skills,
personality, graph, brain, intelligence, and forge -- as thin pass-throughs over
vendored `kleos-lib`, each with a round-trip or smoke test. Re-exports expose the
required kleos-lib types through `henosis_cognition::*` so callers never take a direct
dependency on the vendored crate. All of it stays feature-gated, so default builds
remain ML-free.

Row 4 (handoff facade needed the tenant schema) was FIXED and removed: the facade
gained tenant-backed constructors `open_tenant_memory` / `open_tenant_path` over
`kleos-lib`'s `Database::open_tenant*`, which run the tenant migration chain (handoffs
`schema_v43` included), so the `handoffs_*` pass-throughs now round-trip against a
tenant session (in-memory and path-backed reopen both tested). The monolith lite
session is unchanged and stays memory + scratchpad only.

As of 2026-06-29 every ledger row is closed -- the table below is empty. Row numbers
were kept stable while rows were open because code comments referenced them. Severities
were from the adversarial verification pass.

The 2026-07-20 completion audit found and closed two additional live-composition gaps that the
empty ledger and stub scan had missed. `syntheos-server` now installs `HenosisExecutor` instead of
`DenyExecutor`; it executes ordinary tools through Hermes's shared controlled path, preserving
tenant configuration, rate limiting, circuits, metrics, audit, and Axon, while legacy in-process
credential operations use the same store as the required credential-policy gate.
The server also subscribes once to dispatcher action events and projects every event into Broca,
plus task-correlated events into an append-only Chiasm task-activity projection. The production
gate chain no longer substitutes a deny gate when the credential-policy key is absent; missing
authority configuration is an explicit boot error.

The 2026-07-22 public-readiness review reopened five production-boundary gaps. They do not block
the loopback-only governed mission, but they do block claims that Henosis is ready for an
untrusted remote deployment. The legacy in-process Phylax experiment remains in the server's
credential gate and executor even though `cred`, brokered by `phylaxd`, is the canonical
credential path. Caller-supplied identities, caller-declared approval requirements, mutable audit
storage, and in-process adapters are also explicit active-development limits.

| # | Sev | Not-wired | Where | Closes when |
|---|-----|-----------|-------|-------------|
| 19 | HIGH | The server credential gate and credential operations still use the legacy in-process Phylax store instead of the canonical `cred` and `phylaxd` path. | `crates/syntheos-server/src/main.rs`, `crates/syntheos-server/src/henosis_executor.rs` | Dispatcher policy and executor credential use are brokered through `phylaxd`, then the legacy production dependency is removed. |
| 20 | HIGH | The human gate trusts `args.requires_approval`, and the server exposes no authenticated operator route that resolves pending approvals. | `crates/henosis-rift/src/gate.rs`, `crates/henosis-rift/src/approver.rs`, `crates/syntheos-server/src/main.rs` | Trusted policy marks approval-required actions and an authenticated operator surface resolves approval IDs. |
| 21 | HIGH | Kernel HTTP routes accept caller-asserted tenant and principal identities. | `crates/syntheos-server/src/app.rs` | An authentication boundary derives tenant and principal identities from verified credentials before handlers run. |
| 22 | MEDIUM | Chiasm, Broca, and Hermes audit records are mutable data without a cryptographic or off-host witness. | `crates/syntheos-server/src/action_reactor.rs`, `crates/henosis-hermes/src/audit.rs` | Audit records are hash-chained or signed and anchored in an append-only external witness with verification support. |
| 23 | MEDIUM | Hermes adapters execute in-process without a universal capability sandbox or egress policy. | `crates/henosis-hermes/src/adapters/`, `crates/henosis-hermes/src/lib.rs` | Adapter execution is isolated and every filesystem, process, credential, and network capability is explicitly scoped. |

<!-- Add a row whenever a half-wire is introduced or discovered; delete it when wired. -->

## Dependency advisories (not code half-wires)

The CI dependency gate runs `cargo audit` against the full lockfile. Its sole vulnerability
exception is `RUSTSEC-2023-0071`, whose reviewed scope and removal condition are recorded in
`SECURITY.md`. RustSec informational warnings do not fail `cargo audit`; the current warnings cover
unmaintained `paste` and `ttf-parser`, unsound `lru::IterMut`, and the yanked `spin 0.9.8` release.
