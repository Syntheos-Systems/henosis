# memory

CRUD and hybrid retrieval for the `memories` table. Combines vector ANN (LanceDB), full-text
search (SQLite FTS5), graph link traversal, and a cross-encoder reranker into a unified
4-channel pipeline.

## Key types (`types.rs`)

| Type | Purpose |
|------|---------|
| `Memory` | Full memory row including FSRS fields, valence, tags, version chain pointers |
| `StoreRequest` / `StoreResult` | Input and output for `store` / `store_with_chunks` |
| `UpdateRequest` | Partial update; carries optional new embedding and chunk embeddings |
| `SearchRequest` | Query + optional embedding + filters + strategy hints |
| `SearchResult` | Memory + per-channel scores (semantic, fts, graph, decay, pagerank boost) |
| `FacetedSearchRequest` / `FacetedSearchResponse` | Tag/category/date filter search with facet aggregation |
| `ListOptions` | Pagination + category/source/space filters for `list` |
| `VectorSyncReplayReport` / `BackfillReport` | Outcome of sync-pending replay and embedding backfill |

## Module map

| File | Role |
|------|------|
| `mod.rs` | CRUD: `store`, `store_with_chunks`, `get`, `list`, `update`, `delete`, `insert_link`, tag/version helpers |
| `search.rs` | `hybrid_search`, `hybrid_search_reranked`, `faceted_search`, `auto_link` |
| `fts.rs` | FTS5 tokenization helpers and `fts_search` (BM25) |
| `vector.rs` | SQLite-vec fallback `vector_search` and `chunk_vector_search` |
| `vector_sync.rs` | `replay_vector_sync_pending`, `backfill_missing_embeddings`, LanceDB index rebuild |
| `scoring.rs` | RRF fusion, FSRS decay floor, PageRank boost, source-count boost, recency weight, `AUTO_LINK_THRESHOLD` |
| `simhash.rs` | 64-bit SimHash + Hamming distance for near-duplicate detection on write |
| `auto_tag.rs` | Rule-based tag and category inference for untagged memories |

## Store flow

```
store_with_chunks(req)
  -> embed content (EmbeddingProvider)
  -> chunk_and_embed (overlap-aware chunking)
  -> store(req)
       -> validate content (size, empty)
       -> l2_normalize embedding
       -> enforce quota
       -> simhash duplicate check (Hamming < 3 -> early return)
       -> auto_tag / auto_category if not supplied
       -> INSERT INTO memories (SQLite transaction)
       -> LanceDB insert (vector index)
       -> write_chunks (SQLite + LanceDB chunk index)
       -> mark_pagerank_dirty
       -> store_valence (async, best-effort)
       -> invalidate_search_cache
```

`update` follows the same embedding/LanceDB flow but creates a new version row and marks
the old row `is_latest = 0`. `delete` is a soft delete (`is_forgotten = 1`).

## Search flow (`search.rs`)

`hybrid_search(db, req)` runs a 4-channel pipeline:

1. **Vector** -- LanceDB ANN (falls back to chunk index, then SQLite-vec)
2. **FTS5** -- BM25 keyword search via `fts_search`
3. **Graph** -- 2-hop link expansion from top-scoring seeds (parallel `fetch_graph_neighbors`)
4. **Reranker** -- optional cross-encoder pass via `hybrid_search_reranked`

Results from channels 1-3 are fused with Reciprocal Rank Fusion (RRF), then scored with:
FSRS retrievability decay, PageRank boost, source-count boost, static-memory boost, recency
weight, and contradiction penalty. Results are cached per-user with a 15-second LRU TTL
(2048 entries, 32 shards). Any write invalidates the user's generation counter.

`faceted_search` delegates to `hybrid_search` when a query is present, or runs a direct DB
scan for filter-only requests, then computes tag/category/source/importance facets over the
matched set.

## FSRS-6 fields

Each `Memory` carries `fsrs_stability`, `fsrs_difficulty`, `fsrs_storage_strength`,
`fsrs_retrieval_strength`, `fsrs_learning_state`, `fsrs_reps`, `fsrs_lapses`, and
`fsrs_last_review_at`. The search scorer reads `fsrs_stability` to compute live
retrievability via `crate::fsrs::retrievability`.

## Key files

- `mod.rs` -- CRUD surface and `MEMORY_COLUMNS` / `row_to_memory` (kept in sync by tests)
- `search.rs` -- hybrid pipeline, caching, faceted search, auto-link
- `scoring.rs` -- all scoring constants and helpers
- `vector_sync.rs` -- LanceDB sync-pending ledger replay
