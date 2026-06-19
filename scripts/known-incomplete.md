# Known-incomplete ledger (Henosis)

Everything not fully wired, in one place. `scripts/stub-scan.sh` reprints this every run so
nothing here can hide. If a row exists, that thing is NOT "done" -- do not claim it is. When you
close a row, delete it in the same commit.

Rows 1-5 are deliberately-deferred build-plan wiring. Rows 6-18 were found by the half-wiring
audit (2026-06-18) -- mostly in earlier-absorbed code (Stories 4.1 / 4.4), latent or feature-
deferred, now disclosed. Severities are from the adversarial verification pass.

| # | Sev | Not-wired | Where | Closes when |
|---|-----|-----------|-------|-------------|
| 1 | plan | plutus gate is a deny-stub (fail-closed deny; no quota/RBAC authority) | `syntheos-server/src/app.rs` `live_gate_chain`, `main.rs` | Phase 6 (Plutus authority) |
| 2 | plan | rift bridge memory still HTTP-tunnels to :4200 (`InProcessKleosClient::search_memories`/`store_consensus_memory`) | `henosis-rift-bridge/src/kleos.rs` ~L463/573 | Wave 3 (rewire to `Cognition`) |
| 3 | plan | cognition facade is a PARTIAL surface: memory/context/scratchpad/handoffs only -- no brain/graph/intelligence/personality/skills/forge | `henosis-cognition/src/lib.rs` | later waves |
| 4 | plan | handoff facade methods need the tenant schema (schema_v43); unexercised against the monolith `open_in_memory()` session | `henosis-cognition/src/lib.rs` | Wave 4 (tenant-backed store) |
| 5 | plan | cognition is wired-but-inert + VOLATILE: no route reads `AppState::cognition()`, and `main.rs` opens `open_in_memory()` (state lost on restart) | `syntheos-server/src/{app.rs,main.rs}` | Wave 3 (route) / Wave 3-4 (persistent store) |
| 6 | MED | `cargo_target_dir` config accepted from TOML + documented but never forwarded to any spawned process env -- operator setting is silently ignored | `henosis-rift-bridge/src/config.rs:188` | inject `.env("CARGO_TARGET_DIR")` in sandbox/executor spawn, or remove the field |
| 7 | MED | `prior_context: None` hardcoded; `SynapseExecutor` consumes the field, so prior context is dropped on every supervised/resumed run | `henosis-rift-bridge/src/execution/supervisor.rs:61` | `SupervisedTask` carries prior state and `run()` populates it |
| 8 | MED | `UpdatePresence` WS command never persists status (in-memory only, lost on reconnect); `db::update_user_status` is dead code | `henosis-rift-server/src/ws/gateway.rs:281`, `db/mod.rs:108` | arm calls `update_user_status` before broadcasting |
| 9 | MED | `get_server_members` hardcodes `is_agent:false` + agent fields + `created_at = joined_at`; agent members render as non-agents | `henosis-rift-server/src/db/mod.rs:330` | `MemberWithUser` SELECTs the real agent/timestamp columns |
| 10 | LOW | `list_dms` hardcodes every recipient `status:"offline"` + non-agent; the query omits `u.status`/`u.is_agent` | `henosis-rift-server/src/routes/users.rs:244`, `db/mod.rs:936` | query SELECTs and threads `u.status`, `u.is_agent` |
| 11 | MED | **FAIL-OPEN**: `CheckType::Drift` rule is accepted at supervisor construction with no `InvalidPolicy` and silently never runs at scan time -- violates the crate's fail-closed invariant | `henosis-eidolon/src/supervisor/mod.rs:71` | Phase 4 drift detector, OR reject Drift rules at construction now |
| 12 | MED | raw model-controlled `category`/`source`/`query` interpolated into Kleos URL query strings with no percent-encoding | `synapse-tools/src/kleos.rs:336/339/1693` | percent-encode before interpolation |
| 13 | MED | `ToolCompleted` progress event carries literal `"tool"` placeholder name -- tool-name correlation lost for consumers | `synapse-core/src/executors/synapse_executor.rs:307` | track the `ToolStart` name and reuse it |
| 14 | LOW | `ExecutionResult::Success` always carries `commit_hash:None` (no git integration); evidence/commit checks are meaningless | `synapse-core/src/executors/synapse_executor.rs:338` | probe git HEAD post-run, or doc that synapse-native never populates it |
| 15 | LOW | 401-retry uses `.expect()` on an unsupported HTTP verb (first attempt map_err's it); a future non-GET/POST caller panics on 401 | `syntheos-memory-gateway/src/kleos.rs:151` | retry path `map_err`s like the first attempt |
| 16 | LOW | `SynapseExecutor::sandbox()` returns hardcoded branch `agent/synapse/unset` AND its doc falsely claims it returns the branch from `base_config`; currently unread | `synapse-core/src/executors/synapse_executor.rs:229` | derive a real branch; fix the false doc now |
| 17 | LOW | `SynapseExecutor::health_check()` always returns `Ready` (disclosed placeholder); no production caller yet | `synapse-core/src/executors/synapse_executor.rs:347` | probe provider/Kleos AND a caller invokes it |
| 18 | MED | `synapse-tui` `/model` and `synapse-cli` provider-swap are silent no-ops that drop recognized command args with no user-visible notice | `synapse-tui/src/main.rs:273`, `synapse-cli/src/main.rs:489` | live model/provider switching, or surface a notice until then |

<!-- Add a row whenever a half-wire is introduced or discovered; delete it when wired. -->
