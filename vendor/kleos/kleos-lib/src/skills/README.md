# skills

Versioned, composable agent workflows backed by the `skill_records` table. A skill bundles
a name, prompt/code body, language, trust score, and execution history, and supports
structured evolution through capture, derivation, and fix cycles.

## Key types (`types.rs`)

| Type | Purpose |
|------|---------|
| `Skill` | Full skill row: name, agent, description, code, language, version chain, trust score, counters |
| `CreateSkillRequest` / `UpdateSkillRequest` | Mutation inputs |
| `SkillJudgment` | Scored evaluation from a judge agent; updates `trust_score` as rolling average |
| `ExecutionRecord` | One execution event: success flag, duration, error type/message |
| `EvolutionFeedRow` | Recent evolution event for feed surfaces |
| `SkillOrigin` | `Imported / Captured / Derived / Fixed` -- how the skill was created |
| `SkillCategory` | `ToolGuide / Workflow / Reference` |
| `SkillVisibility` | `Private / Public` |
| `EvolutionType` | `Fix / Derived / Captured` |
| `EvolutionTrigger` | `Analysis / ToolDegradation / MetricMonitor` |
| `ToolQuality` | Per-tool success rate and latency aggregates |

## Module map

| File | Role |
|------|------|
| `mod.rs` | CRUD: `create_skill`, `get_skill`, `list_skills`, `update_skill`, `delete_skill`, `recompute_skill` |
| `mod.rs` | Execution: `record_execution`, `get_executions` |
| `mod.rs` | Judgments: `add_judgment`, `get_judgments` |
| `mod.rs` | Tool quality: `record_tool_quality`, `get_tool_quality` |
| `registry.rs` | Lookup-by-name and registry queries |
| `search.rs` | Keyword and semantic search across skill definitions |
| `analyzer.rs` | Structural analysis: size, complexity, dependency graph shape |
| `evolver.rs` | Controlled mutation and evaluation loops |
| `patch.rs` | Incremental edit application and rollback |
| `dashboard.rs` | Aggregate stats and health summary for UI surfaces |
| `cloud.rs` | Import/export to shared cloud skill libraries |
| `conversation_formatter.rs` | Projects conversations into LLM backend message schemas |

## Data flow

### Capture
```
Agent observes a successful interaction
  -> create_skill(req) with origin = Captured
  -> INSERT skill_records (version=1, trust_score=0)
  -> INSERT skill_tags, skill_tool_deps
  -> INSERT skill_lineage_parents (if derived)
```

### Execution recording
```
Agent runs skill
  -> record_execution(skill_id, success, duration_ms)
  -> INSERT execution_analyses
  -> UPDATE skill_records: success/failure/execution counters, avg_duration_ms
```

### Evolution
```
evolver.rs detects degradation (trigger: AnalysiS / ToolDegradation / MetricMonitor)
  -> mutate code or prompt
  -> create_skill with parent_skill_id = current skill
  -> add_judgment from judge agent -> updates trust_score
  -> recompute_skill resets counters if re-evaluation needed
```

### Trust score
Each `add_judgment` call inserts into `skill_judgments` and recalculates `skill_records.trust_score`
as the average of all judgment scores for that skill. `list_skills` orders by `trust_score DESC`.

## Version chain

Skills version the same way as memories: a `parent_skill_id` + `root_skill_id` chain. Each
mutation creates a new row with an incremented `version`; `skill_lineage_parents` records the
full ancestry graph for feed queries (`list_recent_evolutions`).

## Key files

- `mod.rs` -- CRUD, execution recording, judgments, tool quality, lineage
- `types.rs` -- all shared DTOs and enums
- `evolver.rs` -- mutation and evaluation loops
- `analyzer.rs` -- structural analysis passes
- `search.rs` -- semantic + keyword skill search
