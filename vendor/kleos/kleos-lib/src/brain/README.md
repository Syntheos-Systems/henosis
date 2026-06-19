# brain

Associative memory substrate: a modern Hopfield network with dream-cycle consolidation,
FSRS-6 decay/retrievability, PCA compression, instinct rules, and multi-modal reasoning.

## Submodules

| Submodule | Role |
|-----------|------|
| `hopfield/` | Core Hopfield network: storage, softmax-attention retrieval, reinforcement, decay |
| `dream/` | Six-stage dream cycle for offline consolidation |
| `evolution.rs` | Network-level evolution state and stats |
| `instincts/` | Hard-coded instinct rules that fire on pattern activation |
| `pca.rs` | PCA model fit/transform for dimensionality reduction of embeddings |
| `reasoning.rs` | Multi-modal inference: abductive, predictive, synthesis, rule, analogical |

## Key types (`types.rs`)

| Type | Purpose |
|------|---------|
| `FeedbackSignal` | Usefulness signal for a set of memories and edges |
| `PcaModelRow` | Stored PCA model blob keyed by source/target dims |
| `InferenceKind` | `Abductive / Predictive / Synthesis / Rule / Analogical` |
| `Inference` | A single inference with confidence and supporting memory IDs |
| `ReasoningConfig` | Flags enabling/disabling each reasoning mode |
| `ContradictionPair` | Pair of conflicting Hopfield patterns with activation scores |

## Hopfield network (`hopfield/`)

Uses the modern Hopfield formulation (Ramsauer et al. 2020): continuous embedding vectors,
softmax-attention retrieval, exponential capacity. Core types:

- `HopfieldNetwork` -- matrix store + `DEFAULT_BETA` temperature
- `BrainPattern` -- embedding + strength + activation count, persisted in `brain_patterns`
- `BrainEdge` -- weighted link between patterns, persisted in `brain_edges`
- `RecallResult` -- ranked pattern matches returned by retrieval
- `DecayStats` -- counts from a decay tick run

Key functions (in `hopfield/recall.rs`):
- `store_pattern` -- insert or reinforce a pattern
- `recall_pattern` -- softmax-attention nearest-neighbor lookup
- `decay_tick` -- apply time-based strength decay
- `prune_weak` -- remove patterns below strength threshold
- `merge_similar` -- collapse near-duplicate patterns

## Dream cycle (`dream/`)

Six sequential stages that mirror sleep-phase consolidation:

1. `replay` -- strengthen recently-activated patterns
2. `merge` -- collapse similar patterns into one stronger entry
3. `prune` -- drop below-threshold patterns
4. `discover` -- find new cross-pattern connections
5. `decorrelate` -- reduce redundant edge weights
6. `resolve` -- boost the winner in a contradiction pair

Driver: `dream::run_dream_cycle(db, network, user_id, budget)`. Persists a run record to
`brain_dream_runs` before starting and updates it with stage counts on completion.

## FSRS-6 integration

`brain` relies on `crate::fsrs` for retrievability. The `hopfield/recall.rs` layer reads
`fsrs_stability` from memories and feeds it into `crate::fsrs::retrievability` to bias
pattern activation toward memories that are due for review.

## Key files

- `hopfield/mod.rs` -- public API surface; re-exports `HopfieldNetwork`, `BrainPattern`, etc.
- `hopfield/recall.rs` -- store/recall/reinforce/decay/prune/merge operations
- `dream/mod.rs` -- dream cycle driver
- `reasoning.rs` -- multi-modal inference engine
- `pca.rs` -- PCA fit/transform with SQLite persistence
