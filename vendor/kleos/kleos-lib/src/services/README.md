# services

Named subsystems that give agents a shared coordination layer: life-cycle management,
event publishing, task routing, action logging, and metrics/evaluation.

## Service map

| Module | Biological analogy | Role |
|--------|--------------------|------|
| `soma.rs` | soma (cell body) | Agent life-cycle: register, heartbeat, status, quality score |
| `axon.rs` | axon (signal carrier) | Event bus: publish, subscribe, cursor-based replay |
| `chiasm.rs` | optic chiasm (routing) | Task routing: create, update, list tasks and task updates |
| `broca.rs` | Broca's area (speech/action) | Action log: record what an agent did and why |
| `thymus.rs` | thymus (immune/quality) | Metrics, rubrics, evaluations, health assessments |
| `loom.rs` | weaving together | Shared utilities used across service modules |
| `brain.rs` | brain coordination | Service-layer bridge into the `brain` module |

## Key types by service

### soma -- Agent life-cycle
- `Agent` -- registered agent row: name, type, capabilities, status, heartbeat, quality score, drift flags
- `RegisterAgentRequest` -- input for registration
- `SomaStats` -- total/online agent counts and type count

### axon -- Event bus
- `Event` -- published event: channel, action, JSON payload, source, agent
- `PublishEventRequest` -- input for publish
- `Channel` / `Subscription` / `Cursor` -- channel management and cursor-based replay
- `AxonStats` -- total events, channel count, source count

### chiasm -- Task routing
- `Task` -- a unit of work owned by an agent in a project, with status lifecycle
- `TaskUpdate` -- status transition record for a task
- `CreateTaskRequest` / `UpdateTaskRequest` -- mutation inputs
- `ChiasmStats` -- total count and breakdown by status

### broca -- Action log
- `ActionEntry` -- log row: agent, service, action name, JSON payload, optional narrative, optional axon event link
- `LogActionRequest` -- input for recording an action
- `BrocaStats` -- total actions, distinct agents, distinct services

### thymus -- Metrics and evaluation
- `Rubric` / `CreateRubricRequest` / `UpdateRubricRequest` -- structured evaluation criteria
- `Evaluation` / `EvaluateRequest` -- scored agent evaluation against a rubric
- Health check and system metric types (see `thymus.rs` for full list)

## Data flow

```
Agent calls POST /activity (server fan-out endpoint)
  -> chiasm: create or update task
  -> axon: publish event to channel
  -> broca: log the action
  -> thymus: record metric
  -> memory/skills: store to Kleos, match skills
```

Each service operates independently on its own DB tables. `axon` events carry an optional
`axon_event_id` that `broca` stores so action log entries can be linked back to the event
that triggered them.

Agents register via `soma`, heartbeat to stay `online`, and are marked `offline` or `error`
when heartbeats lapse. `thymus` evaluations reference a `Rubric` and score an agent's output
against structured criteria; evaluation results feed back into `soma`'s `quality_score`.

## Key files

- `soma.rs` -- agent registration, heartbeat, status transitions
- `axon.rs` -- event publish/subscribe/replay
- `chiasm.rs` -- task CRUD and status updates
- `broca.rs` -- action logging
- `thymus.rs` -- rubrics, evaluations, health metrics
