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
Row numbers are kept stable because code comments reference them. Severities are from the
adversarial verification pass.

| # | Sev | Not-wired | Where | Closes when |
|---|-----|-----------|-------|-------------|
| 1 | plan | plutus gate is a deny-stub (fail-closed deny; no quota/RBAC authority) | `syntheos-server/src/app.rs` `live_gate_chain`, `main.rs` | Phase 6 (Plutus authority) |
| 3 | plan | cognition facade is a PARTIAL surface: memory/context/scratchpad/handoffs only -- no brain/graph/intelligence/personality/skills/forge | `henosis-cognition/src/lib.rs` | later waves |
| 4 | plan | handoff facade methods need the tenant schema (schema_v43); unexercised against the monolith session (`open_in_memory`/`open_path` both run the monolith migration chain, not the tenant one) | `henosis-cognition/src/lib.rs` | Wave 4 (tenant-backed store) |

<!-- Add a row whenever a half-wire is introduced or discovered; delete it when wired. -->

## Dependency advisories (not code half-wires)

GitHub Dependabot remediation (2026-06-22). The rustls-webpki HIGH + 2 LOW, the SQLx MED, and the
jsonwebtoken 3x MED were fixed by bumping the Postgres stack to sqlx 0.8.6 (via the vendored
Postgres-only facade in `vendor/sqlx`, see its VENDOR.md) and jsonwebtoken to 10 (`rust_crypto`).
One advisory remains, blocked upstream:

| Advisory | Sev | Crate | Why unfixed | Closes when |
|----------|-----|-------|-------------|-------------|
| GHSA-rhfx-m35p-ff5j | LOW | `lru 0.12.5` | Pulled transitively by `ratatui 0.29` (synapse-tui) and `tantivy 0.24` (lancedb <- kleos-lib, cognition). Both pin `lru ^0.12`; the fix lands in `0.16.3`, unreachable without major ratatui/tantivy bumps and a vendored-kleos-lib edit. The bug is a Miri-level Stacked-Borrows UB in `IterMut`, not reachable in our usage. | ratatui/tantivy advance their `lru` pin (or kleos-lib's vendored version moves) |
