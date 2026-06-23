# Known-incomplete ledger (Henosis)

Everything not fully wired, in one place. `scripts/stub-scan.sh` reprints this every run so
nothing here can hide. If a row exists, that thing is NOT "done" -- do not claim it is. When you
close a row, delete it in the same commit.

Rows 1-5 are deliberately-deferred build-plan wiring. Rows 6-18 came from the half-wiring audit
(2026-06-18). Rows 11 (eidolon Drift fail-open), 12 (URL query encoding), and 15 (gateway
401-retry panic) were FIXED and removed; Wave 3 closed row 5 (server cognition route + persistent
store) and narrowed row 2 (bridge memory now runs in-process under `--features cognition`); rows 8
(UpdatePresence now persists via `update_user_status`) and 9 (`get_server_members` now SELECTs the
real agent/timestamp columns) were FIXED and removed; row 6 (`cargo_target_dir` now forwarded to
the executor spawn via `ExecutionSandbox.cargo_target_dir`) was FIXED and removed, and row 7 was
narrowed (the supervisor now threads `prior_context`; only the unbuilt resume data-source remains);
row 13 (`ToolCompleted` now recovers the real tool name by correlating `ToolResult` to `ToolStart`
on the shared tool-call id) was FIXED and removed. Row numbers are kept stable because code
comments reference them. Severities are from the adversarial verification pass.

| # | Sev | Not-wired | Where | Closes when |
|---|-----|-----------|-------|-------------|
| 1 | plan | plutus gate is a deny-stub (fail-closed deny; no quota/RBAC authority) | `syntheos-server/src/app.rs` `live_gate_chain`, `main.rs` | Phase 6 (Plutus authority) |
| 2 | LOW | bridge in-process memory runs on the cognition store ONLY under `--features cognition`; the default build's in-process mode still HTTP-tunnels memory to :4200 (`HttpMemoryBackend`) -- cognition is optional, mirroring the server | `henosis-rift-bridge/src/{kleos.rs,main.rs}` (`BridgeMemory` seam) | the bridge ships with the cognition feature on by default, or HTTP memory is removed |
| 3 | plan | cognition facade is a PARTIAL surface: memory/context/scratchpad/handoffs only -- no brain/graph/intelligence/personality/skills/forge | `henosis-cognition/src/lib.rs` | later waves |
| 4 | plan | handoff facade methods need the tenant schema (schema_v43); unexercised against the monolith session (`open_in_memory`/`open_path` both run the monolith migration chain, not the tenant one) | `henosis-cognition/src/lib.rs` | Wave 4 (tenant-backed store) |
| 7 | LOW | supervisor now THREADS `prior_context` into the executor (`ExecutionSandbox.cargo_target_dir` analog) -- no longer dropped -- but no caller populates it: the crash-recovery resume path that would supply a partial-work summary is unbuilt, so it is `None` in practice | `henosis-rift-bridge/src/room.rs:646` (first-attempt caller) | a resume path detects partial work and passes it as `prior_context` |
| 10 | LOW | `list_dms` hardcodes every recipient `status:"offline"` + non-agent; the query omits `u.status`/`u.is_agent` | `henosis-rift-server/src/routes/users.rs:244`, `db/mod.rs:936` | query SELECTs and threads `u.status`, `u.is_agent` |
| 14 | LOW | `ExecutionResult::Success` always carries `commit_hash:None` (no git integration); evidence/commit checks are meaningless | `synapse-core/src/executors/synapse_executor.rs:338` | probe git HEAD post-run, or doc that synapse-native never populates it |
| 16 | LOW | `SynapseExecutor::sandbox()` returns a hardcoded placeholder branch and is unread by the bridge (doc now accurate; real branch = `sandbox::branch_name`) | `synapse-core/src/executors/synapse_executor.rs:227` | executor owns sandbox derivation AND a caller uses it |
| 17 | LOW | `SynapseExecutor::health_check()` always returns `Ready` (disclosed placeholder); no production caller yet | `synapse-core/src/executors/synapse_executor.rs:347` | probe provider/Kleos AND a caller invokes it |
| 18 | MED | `synapse-tui` `/model` and `synapse-cli` provider-swap are silent no-ops that drop recognized command args with no user-visible notice | `synapse-tui/src/main.rs:273`, `synapse-cli/src/main.rs:489` | live model/provider switching, or surface a notice until then |

<!-- Add a row whenever a half-wire is introduced or discovered; delete it when wired. -->

## Dependency advisories (not code half-wires)

GitHub Dependabot remediation (2026-06-22). The rustls-webpki HIGH + 2 LOW, the SQLx MED, and the
jsonwebtoken 3x MED were fixed by bumping the Postgres stack to sqlx 0.8.6 (via the vendored
Postgres-only facade in `vendor/sqlx`, see its VENDOR.md) and jsonwebtoken to 10 (`rust_crypto`).
One advisory remains, blocked upstream:

| Advisory | Sev | Crate | Why unfixed | Closes when |
|----------|-----|-------|-------------|-------------|
| GHSA-rhfx-m35p-ff5j | LOW | `lru 0.12.5` | Pulled transitively by `ratatui 0.29` (synapse-tui) and `tantivy 0.24` (lancedb <- kleos-lib, cognition). Both pin `lru ^0.12`; the fix lands in `0.16.3`, unreachable without major ratatui/tantivy bumps and a vendored-kleos-lib edit. The bug is a Miri-level Stacked-Borrows UB in `IterMut`, not reachable in our usage. | ratatui/tantivy advance their `lru` pin (or kleos-lib's vendored version moves) |
