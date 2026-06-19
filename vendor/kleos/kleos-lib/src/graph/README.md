# graph

Entity/relationship graph over the memory store: PageRank scoring, community detection,
structural analysis, co-occurrence mining, and visualization-ready graph construction.

## Key types (`types.rs`)

| Type | Purpose |
|------|---------|
| `GraphNode` | Node for visualization: id, label, weight, pagerank, community, category, importance, decay |
| `GraphEdge` | Directed edge with `LinkType` and weight |
| `LinkType` | `Cite / Mentions / Contradicts / Refines / Generalizes / HasFact / Association / Temporal / Causal / Resolves` |
| `MemoryLink` | Low-level link row from `memory_links` |
| `Entity` | Named entity: name, type, description, aliases, confidence, occurrence count |
| `EntityRelationship` | Typed, weighted link between two entities |
| `GraphBuildResult` | Nodes + edges ready for the GUI graph view |
| `PageRankResult` | Map of memory ID to PageRank score |
| `CommunitiesResult` / `CommunityStats` | Louvain community detection output |
| `StructuralNode` / various analysis types | Output of the structural analysis engine |

## Module map

| File | Role |
|------|------|
| `builder.rs` | Builds `GraphBuildResult` (nodes + edges) from `memory_links` for GUI rendering |
| `entities.rs` | CRUD for `entities` and `entity_relationships` tables |
| `pagerank.rs` | Iterative weighted PageRank; dirty-flag cache; `compute_pagerank`, `ensure_pagerank_for_user` |
| `communities.rs` | Louvain-style community detection over the memory link graph |
| `structural.rs` | Deep analysis: centrality, betweenness, bridge detection, path tracing, EN (engram notation) parsing |
| `search.rs` | Graph-aware memory search (entity-anchored, relationship traversal) |
| `cooccurrence.rs` | Tag and entity co-occurrence matrix mining |

## Data flow

### Write path
```
memory::store / update / delete / insert_link
  -> mark_pagerank_dirty(db, user_id)
```
PageRank is not recomputed on every write. A dirty flag is set; the next call to
`ensure_pagerank_for_user` (invoked from `hybrid_search` before scoring) computes and
persists fresh scores to `memories.pagerank_score` if the cache is stale.

### PageRank scoring
```
compute_pagerank(db, user_id, damping=0.85, max_iterations)
  -> collect all non-forgotten memory IDs
  -> load memory_links with edge weights (link_type * similarity)
  -> iterate: score[i] = (1-d)/N + d * sum(score[j] / out_degree[j])
  -> persist scores to memories.pagerank_score
  -> return PageRankResult { scores: HashMap<i64, f64> }
```

Edge weights are typed: `caused_by/causes` = 2.0, `updates/corrects` = 1.5,
`extends/contradicts` = 1.3, `consolidates` = 0.5, default = 1.0, all multiplied by link
similarity.

### Graph build (visualization)
```
builder::build_graph(db, opts)
  -> SELECT memories filtered to user/space
  -> SELECT memory_links joining both sides
  -> map to GraphNode / GraphEdge (with community + pagerank fields)
  -> return GraphBuildResult
```

### Community detection
`communities.rs` runs a graph-partitioning pass over `memory_links` and writes community
IDs back to the `memories` table. `CommunityStats` aggregates per-community importance and
category spread.

### Structural analysis (`structural.rs`)
Provides centrality ranking, betweenness centrality, bridge detection (edges whose removal
disconnects the graph), path tracing between two nodes, role classification (`Hub / Bridge /
Leaf / Isolated / Connector`), and EN (engram notation) parsing for declarative graph
mutation scripts.

### Entity graph (`entities.rs`)
Named entities extracted from memories are stored in `entities` with typed relationships in
`entity_relationships`. `entities.rs` provides CRUD, occurrence tracking, and relationship
strength aggregation. `EntityMemorySearchResult` links entities back to the memories that
mention them.

## Key files

- `pagerank.rs` -- scoring engine, dirty-flag cache, `ensure_pagerank_for_user`
- `builder.rs` -- visualization-ready graph construction
- `structural.rs` -- deep analysis: centrality, bridges, paths, EN notation
- `entities.rs` -- entity/relationship CRUD
- `communities.rs` -- Louvain community detection
- `types.rs` -- all shared DTOs
