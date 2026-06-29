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
syntheos-server, so the default build stays ML-free; KLEOS :4200 coexists permanently (Zan,
2026-06-18), so routing default-build memory to :4200 over HTTP is a chosen tradeoff, not a TODO.
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

| # | Sev | Not-wired | Where | Closes when |
|---|-----|-----------|-------|-------------|

<!-- Add a row whenever a half-wire is introduced or discovered; delete it when wired. -->

## Dependency advisories (not code half-wires)

GitHub Dependabot remediation (2026-06-22). The rustls-webpki HIGH + 2 LOW, the SQLx MED, and the
jsonwebtoken 3x MED were fixed by bumping the Postgres stack to sqlx 0.8.6 (via the vendored
Postgres-only facade in `vendor/sqlx`, see its VENDOR.md) and jsonwebtoken to 10 (`rust_crypto`).
Two advisories remain, both blocked upstream behind the pristine vendored `kleos-lib`
(the opentelemetry one surfaced 2026-06-29 from a fresh disclosure on a pre-existing
transitive dep, not introduced by any Henosis change):

| Advisory | Sev | Crate | Why unfixed | Closes when |
|----------|-----|-------|-------------|-------------|
| GHSA-w9wp-h8wv-79jx | MED | `opentelemetry_sdk 0.27.1` | Pulled transitively by the vendored `kleos-lib` (`opentelemetry` / `opentelemetry-otlp` / `tracing-opentelemetry`), which is PRISTINE and cannot be bumped without a re-vendor. The bug is unbounded memory allocation in W3C Baggage propagation -- a code path Henosis never exercises: `kleos-lib` compiles only under `--features cognition`, and the facade uses its memory/search/context surface, not OTel baggage. | the vendored `kleos-lib` advances its opentelemetry pin (or upstream kleos-lib bumps it) |
| GHSA-rhfx-m35p-ff5j | LOW | `lru 0.12.5` | Pulled transitively by `ratatui 0.29` (synapse-tui) and `tantivy 0.24` (lancedb <- kleos-lib, cognition). Both pin `lru ^0.12`; the fix lands in `0.16.3`, unreachable without major ratatui/tantivy bumps and a vendored-kleos-lib edit. The bug is a Miri-level Stacked-Borrows UB in `IterMut`, not reachable in our usage. | ratatui/tantivy advance their `lru` pin (or kleos-lib's vendored version moves) |
