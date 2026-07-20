//! `henosis-cognition`: a thin, in-process facade over the vendored, untouched
//! `kleos-lib` cognitive core.
//!
//! This crate exposes the cognitive primitives Henosis needs -- memory storage
//! and retrieval, budget-aware context assembly, and a few storage-organism
//! pass-throughs (scratchpad, handoffs) -- as plain async methods on one
//! [`Cognition`] handle. It adds NO behavior of its own: every method forwards
//! to a `kleos-lib` free function or store with the call's exact signature.
//!
//! The "lightweight session" is a runtime composition, not a server:
//! [`Cognition::open_in_memory`] (and [`Cognition::connect`] over a configured
//! [`kleos_lib::config::Config`]) construct a [`kleos_lib::db::Database`] and
//! nothing else. `Database::connect_with_config` runs migrations and opens the
//! pools/indices then returns -- it spawns NO background loops (those live in
//! `kleos-server`, which is NOT vendored). So a facade that constructs a
//! `Database` and calls the core free functions is, by construction, "kleos
//! within Henosis without the whole stack".
//!
//! The embedder is OPTIONAL. With no embedder, [`Cognition::memory_search`] uses
//! the FTS path and [`Cognition::assemble_context`] falls back to its FTS /
//! recency layers; neither constructs an ONNX session, so the native
//! `libonnxruntime` is not required to build, test, or run the lite session.
//!
//! Two session flavors are available: **monolith** (`open_in_memory` /
//! `open_path`) runs the system migration chain and carries memory + scratchpad
//! tables; **tenant** (`open_tenant_memory` / `open_tenant_path`) runs the
//! tenant migration chain and additionally carries the handoffs table set
//! (`schema_v43`) as well as skills, brain, forge, graph, and personality tables.

use std::sync::Arc;

use kleos_lib::config::Config;
use kleos_lib::db::Database;
use kleos_lib::embeddings::onnx::OnnxProvider;
use kleos_lib::handoffs::HandoffsDb;
use tokio::sync::Semaphore;

// Re-export every `kleos-lib` type that appears in this facade's public API, so
// embedding applications (syntheos-server, henosis-rift-bridge) can construct
// requests and read results through `henosis_cognition::*` without taking a
// direct dependency on the vendored `kleos-lib`.
pub use kleos_lib::context::types::{ContextOptions, ContextResult};
pub use kleos_lib::embeddings::EmbeddingProvider;
pub use kleos_lib::handoffs::{
    Handoff, HandoffFilters, HandoffStats, SearchResult as HandoffSearchResult,
    StoreParams as HandoffStoreParams, StoreResult as HandoffStoreResult,
};
pub use kleos_lib::llm::local::LocalModelClient;
pub use kleos_lib::memory::types::{
    ListOptions, Memory, SearchRequest, SearchResult, StoreRequest, StoreResult,
};
pub use kleos_lib::scratchpad::ScratchEntry;
pub use kleos_lib::skills::{
    CreateSkillRequest, EvolutionFeedRow, ExecutionRecord, Skill, SkillJudgment, SkillKind,
    ToolQuality, UpdateSkillRequest,
};
pub use kleos_lib::personality::{StoredProfile, StoredSignal};
pub use kleos_lib::graph::types::{
    CreateEntityRequest, CreateRelationshipRequest, Entity, EntityRelationship,
    EntityMemorySearchResult, GraphBuildOptions, GraphBuildResult, CommunitiesResult,
    CommunityStats, PageRankResult, PageRankUpdateResult,
};
pub use kleos_lib::brain::hopfield::{BrainEdge, BrainPattern, EdgeType, RecallResult};
pub use kleos_lib::intelligence::types::{
    CausalAncestor, CausalChain, CausalLink, Contradiction, Digest, MemoryHealthReport, Reflection,
};
pub use kleos_lib::forge::approaches::ApproachItem;
pub use kleos_lib::Result as KleosResult;

/// The default single-user id for the lightweight session. Kleos memory rows are
/// owner-scoped (`user_id`); the lite session is single-user, so unset request
/// owners default to this id.
pub const DEFAULT_USER_ID: i64 = 1;

/// Errors surfaced by the facade. A thin wrapper over `kleos-lib`'s `EngError`
/// so callers see one error type without depending on `kleos-lib` directly.
#[derive(Debug, thiserror::Error)]
pub enum CognitionError {
    /// An error returned by an underlying `kleos-lib` operation.
    #[error(transparent)]
    Kleos(#[from] kleos_lib::EngError),
}

/// The facade's result type.
pub type Result<T> = std::result::Result<T, CognitionError>;

/// Build the vendored Kleos bge-m3 provider from the process's standard
/// Kleos/Engram environment configuration. Callers can clone the returned
/// `Arc` into every in-process consumer so cognition and agent subsystems use
/// one ONNX session and one vector space.
pub async fn load_embedding_provider_from_env() -> Result<Arc<dyn EmbeddingProvider>> {
    let config = Config::from_env();
    let provider = OnnxProvider::new(&config).await?;
    Ok(Arc::new(provider))
}

/// In-process handle over the `kleos-lib` cognitive core.
///
/// Cheap to clone behind an `Arc` by the embedding application; the inner
/// `Database` is the single shared connection-pool owner.
pub struct Cognition {
    /// The opened `kleos-lib` database (pools + optional vector indices). All
    /// pass-through methods borrow `&Database` from here.
    db: Arc<Database>,
    /// Optional embedding provider. `None` selects the embedding-free paths:
    /// FTS-only search and FTS / recency context assembly.
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Optional local LLM client used only by [`assemble_context`](Cognition::assemble_context)'s
    /// inference layer. `None` skips that layer.
    llm: Option<Arc<LocalModelClient>>,
    /// The owner id applied to memory operations whose request leaves `user_id`
    /// unset (single-user lite session; defaults to [`DEFAULT_USER_ID`]).
    user_id: i64,
    /// Semaphore throttling concurrent handoff auto-GC spawns, shared across the
    /// [`HandoffsDb`] handles this facade builds on demand (mirrors the
    /// kleos-server wiring of [`HandoffsDb::new`]).
    handoff_gc_sem: Arc<Semaphore>,
}

/// Constructs and operates the in-process Kleos cognitive facade.
impl Cognition {
    /// Open the cognitive core over a configured [`Config`], with NO embedder.
    ///
    /// Builds on [`Database::connect_with_config`], which runs migrations and
    /// opens pools/indices and then returns -- spawning no background loops.
    /// The resulting handle uses the embedding-free search / context paths;
    /// attach an embedder with [`with_embedder`](Cognition::with_embedder).
    pub async fn connect(config: &Config) -> Result<Self> {
        let db = Database::connect_with_config(config, None).await?;
        Ok(Self::from_database(db))
    }

    /// Open an in-memory cognitive core for tests and lightweight library use.
    ///
    /// Backed by [`Database::connect_memory`]: a shared-cache in-memory SQLite
    /// with migrations applied and no vector index, embedder, ONNX, or LanceDB.
    /// This is the "kleos within Henosis without the whole stack" entry point.
    pub async fn open_in_memory() -> Result<Self> {
        let db = Database::connect_memory().await?;
        Ok(Self::from_database(db))
    }

    /// Open a tenant-backed in-memory cognitive core for tests and lightweight
    /// library use.
    ///
    /// Backed by [`Database::open_tenant_memory`], which runs the TENANT migration
    /// chain (not the monolith one), so the handoffs table set (`schema_v43`) is
    /// present and the `handoffs_*` pass-throughs are meaningful. Use this instead
    /// of [`open_in_memory`](Cognition::open_in_memory) whenever a session needs
    /// handoffs; the monolith lite session carries only memory + scratchpad tables.
    pub async fn open_tenant_memory() -> Result<Self> {
        let db = Database::open_tenant_memory().await?;
        Ok(Self::from_database(db))
    }

    /// Open a path-backed lite session: a persistent SQLite store at `db_path`
    /// with NO embedder and NO Lance/ONNX vector index, so stored memory and
    /// scratchpad state survive a restart. This is the durable counterpart to
    /// [`open_in_memory`](Cognition::open_in_memory).
    ///
    /// The config disables the Lance vector index (`use_lance_index = false`),
    /// keeping the session on the FTS-only search / recency context paths -- so
    /// no LanceDB table is opened and the native ONNX runtime is not required.
    /// Migrations run on open, so a fresh file lands at the current monolith
    /// schema and an existing file is upgraded in place.
    pub async fn open_path(db_path: &str) -> Result<Self> {
        let mut config = Config::default();
        config.db_path = db_path.to_string();
        config.use_lance_index = false;
        Self::connect(&config).await
    }

    /// Open a durable tenant-backed session at `db_path`, owned by `owner_user_id`.
    ///
    /// Backed by [`Database::open_tenant`] with no vector index and no encryption,
    /// so the file lands at the current TENANT schema (handoffs `schema_v43`
    /// included) and is upgraded in place on reopen. `owner_user_id` is threaded
    /// into the tenant migration chain so the memory-core `user_id` re-add can
    /// backfill existing rows to the owner. This is the durable counterpart to
    /// [`open_tenant_memory`](Cognition::open_tenant_memory); use it whenever a
    /// persistent session needs handoffs.
    pub async fn open_tenant_path(db_path: &str, owner_user_id: i64) -> Result<Self> {
        let db = Database::open_tenant(db_path, None, None, Some(owner_user_id)).await?;
        Ok(Self::from_database(db).with_user_id(owner_user_id))
    }

    /// Build the handle from an already-opened database (no embedder, no llm).
    fn from_database(db: Database) -> Self {
        Self {
            db: Arc::new(db),
            embedder: None,
            llm: None,
            user_id: DEFAULT_USER_ID,
            handoff_gc_sem: Arc::new(Semaphore::new(1)),
        }
    }

    /// Attach an embedding provider, enabling the vector / hybrid search and the
    /// embedding-scored context layers. Builder-style; returns `self`.
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedder = Some(embedder);
        self
    }

    /// Attach a local LLM client, enabling the context-assembly inference layer.
    /// Builder-style; returns `self`.
    pub fn with_llm(mut self, llm: Arc<LocalModelClient>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Override the default owner id used for memory operations. Builder-style.
    pub fn with_user_id(mut self, user_id: i64) -> Self {
        self.user_id = user_id;
        self
    }

    /// The configured default owner id.
    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// The shared database handle, for callers that need the raw `kleos-lib`
    /// store (e.g. to construct another `kleos-lib` facade over the same pools).
    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    // -- Gate-critical core ---------------------------------------------------

    /// Store a memory. Defaults the request owner to the session user when
    /// unset. With an embedder attached and no pre-supplied embedding, routes
    /// through [`kleos_lib::memory::store_with_chunks`] so vector + chunk
    /// embeddings are computed; otherwise stores directly (FTS-only path).
    pub async fn memory_store(&self, mut req: StoreRequest) -> Result<StoreResult> {
        if req.user_id.is_none() {
            req.user_id = Some(self.user_id);
        }
        let result = match (&self.embedder, req.embedding.is_some()) {
            (Some(embedder), false) => {
                kleos_lib::memory::store_with_chunks(&self.db, embedder.as_ref(), req).await?
            }
            _ => kleos_lib::memory::store(&self.db, req, None, false).await?,
        };
        Ok(result)
    }

    /// Hybrid search over memories. Defaults the request owner to the session
    /// user when unset. With an embedder attached and no pre-supplied embedding,
    /// embeds the query first (vector + FTS); with no embedder the embedding
    /// stays `None` and the search runs FTS-only.
    pub async fn memory_search(&self, mut req: SearchRequest) -> Result<Arc<Vec<SearchResult>>> {
        if req.user_id.is_none() {
            req.user_id = Some(self.user_id);
        }
        if req.embedding.is_none() && !req.query.is_empty() {
            if let Some(embedder) = &self.embedder {
                // Best-effort: an embedding failure leaves the request on the
                // FTS-only path rather than failing the whole search.
                if let Ok(emb) = embedder.embed(&req.query).await {
                    req.embedding = Some(emb);
                }
            }
        }
        let results = kleos_lib::memory::search::hybrid_search(&self.db, req).await?;
        Ok(results)
    }

    /// Assemble a budget-aware context window for the session user. Forwards the
    /// facade's optional embedder and llm; with `embedder = None` the assembler
    /// falls back to its FTS / recency layers (no ONNX session is built).
    pub async fn assemble_context(&self, opts: ContextOptions) -> Result<ContextResult> {
        let result = kleos_lib::context::assemble_context(
            &self.db,
            opts,
            self.user_id,
            self.embedder.clone(),
            self.llm.clone(),
        )
        .await?;
        Ok(result)
    }

    // -- Memory CRUD pass-throughs --------------------------------------------

    /// Fetch one active memory by id for the session user.
    pub async fn memory_get(&self, id: i64) -> Result<Memory> {
        Ok(kleos_lib::memory::get(&self.db, id, self.user_id).await?)
    }

    /// List active memories for the session user. Defaults the options owner to
    /// the session user when unset.
    pub async fn memory_list(&self, mut opts: ListOptions) -> Result<Vec<Memory>> {
        if opts.user_id.is_none() {
            opts.user_id = Some(self.user_id);
        }
        Ok(kleos_lib::memory::list(&self.db, opts).await?)
    }

    // -- Scratchpad pass-throughs ---------------------------------------------

    /// Insert or update one scratchpad entry with a TTL (minutes).
    pub async fn scratchpad_put(
        &self,
        session: &str,
        agent: &str,
        model: &str,
        key: &str,
        value: &str,
        ttl_minutes: i64,
    ) -> Result<()> {
        Ok(kleos_lib::scratchpad::upsert_entry(
            &self.db, session, agent, model, key, value, ttl_minutes,
        )
        .await?)
    }

    /// List active scratchpad entries filtered by agent, model, and session.
    pub async fn scratchpad_list(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
        session: Option<&str>,
    ) -> Result<Vec<ScratchEntry>> {
        Ok(kleos_lib::scratchpad::list_entries(&self.db, agent, model, session).await?)
    }

    /// Load every entry for one scratchpad session in creation order.
    pub async fn scratchpad_get_session(&self, session: &str) -> Result<Vec<ScratchEntry>> {
        Ok(kleos_lib::scratchpad::get_session_entries(&self.db, session).await?)
    }

    /// Look up a single non-expired scratchpad value by namespace and key.
    pub async fn scratchpad_get(&self, namespace: &str, key: &str) -> Result<Option<String>> {
        Ok(kleos_lib::scratchpad::get_by_namespace_key(&self.db, namespace, key).await?)
    }

    /// Delete every scratchpad entry for one session.
    pub async fn scratchpad_delete_session(&self, session: &str) -> Result<()> {
        Ok(kleos_lib::scratchpad::delete_session(&self.db, session).await?)
    }

    /// Delete one key from one scratchpad session.
    pub async fn scratchpad_delete_key(&self, session: &str, key: &str) -> Result<()> {
        Ok(kleos_lib::scratchpad::delete_session_key(&self.db, session, key).await?)
    }

    // -- Handoff pass-throughs ------------------------------------------------

    /// Build a [`HandoffsDb`] over the shared database and gc semaphore.
    ///
    /// The handoffs table set ships in the tenant schema (`schema_v43`), so these
    /// pass-throughs are meaningful only on a tenant-backed [`Database`] -- open
    /// the session with [`open_tenant_memory`](Cognition::open_tenant_memory) or
    /// [`open_tenant_path`](Cognition::open_tenant_path). The monolith lite session
    /// ([`open_in_memory`](Cognition::open_in_memory) / [`open_path`](Cognition::open_path))
    /// carries only memory + scratchpad tables, and a handoff call against it errors.
    fn handoffs(&self) -> HandoffsDb {
        HandoffsDb::new(self.db.clone(), self.handoff_gc_sem.clone())
    }

    /// Store a handoff for the session user.
    pub async fn handoffs_store(&self, params: HandoffStoreParams) -> Result<HandoffStoreResult> {
        Ok(self.handoffs().store(params, self.user_id).await?)
    }

    /// List handoffs for the session user under the given filters.
    pub async fn handoffs_list(&self, filters: HandoffFilters) -> Result<Vec<Handoff>> {
        Ok(self.handoffs().list(filters, self.user_id).await?)
    }

    /// Fetch the latest handoff matching the filters for the session user.
    pub async fn handoffs_latest(&self, filters: HandoffFilters) -> Result<Option<Handoff>> {
        Ok(self.handoffs().get_latest(filters, self.user_id).await?)
    }

    /// Full-text search handoffs for the session user.
    pub async fn handoffs_search(
        &self,
        query: &str,
        project: Option<&str>,
        limit: i64,
    ) -> Result<Vec<HandoffSearchResult>> {
        Ok(self
            .handoffs()
            .search(query, project, limit, self.user_id)
            .await?)
    }

    /// Aggregate handoff statistics for the session user.
    pub async fn handoffs_stats(&self) -> Result<HandoffStats> {
        Ok(self.handoffs().stats(self.user_id).await?)
    }

    // -- Skills pass-throughs --------------------------------------------------

    /// Create a skill for the session user. Forwards to `kleos_lib::skills::create_skill`.
    /// Sets `req.user_id` to the session user when the caller leaves it unset.
    pub async fn skill_create(&self, mut req: CreateSkillRequest) -> Result<Skill> {
        if req.user_id.is_none() {
            req.user_id = Some(self.user_id);
        }
        Ok(kleos_lib::skills::create_skill(&self.db, req).await?)
    }

    /// Fetch one skill by id for the session user.
    pub async fn skill_get(&self, id: i64) -> Result<Skill> {
        Ok(kleos_lib::skills::get_skill(&self.db, id, self.user_id).await?)
    }

    /// List active skills for the session user. Optionally filter by agent.
    pub async fn skill_list(
        &self,
        agent: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Skill>> {
        Ok(kleos_lib::skills::list_skills(&self.db, self.user_id, agent, limit, offset).await?)
    }

    /// Apply a partial update to a skill owned by the session user.
    pub async fn skill_update(&self, id: i64, req: UpdateSkillRequest) -> Result<Skill> {
        Ok(kleos_lib::skills::update_skill(&self.db, id, req, self.user_id).await?)
    }

    /// Recompute a skill's derived fields for the session user.
    pub async fn skill_recompute(&self, id: i64) -> Result<Skill> {
        Ok(kleos_lib::skills::recompute_skill(&self.db, id, self.user_id).await?)
    }

    /// Delete a skill owned by the session user.
    pub async fn skill_delete(&self, id: i64) -> Result<()> {
        Ok(kleos_lib::skills::delete_skill(&self.db, id, self.user_id).await?)
    }

    /// Record one execution attempt for a skill owned by the session user.
    pub async fn skill_record_execution(
        &self,
        skill_id: i64,
        success: bool,
        duration_ms: Option<f64>,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) -> Result<()> {
        Ok(kleos_lib::skills::record_execution(
            &self.db,
            skill_id,
            self.user_id,
            success,
            duration_ms,
            error_type,
            error_message,
        )
        .await?)
    }

    /// Fetch execution history for a skill owned by the session user.
    pub async fn skill_get_executions(
        &self,
        skill_id: i64,
        limit: usize,
    ) -> Result<Vec<ExecutionRecord>> {
        Ok(kleos_lib::skills::get_executions(&self.db, skill_id, self.user_id, limit).await?)
    }

    /// Add a judgment score to a skill owned by the session user.
    pub async fn skill_add_judgment(
        &self,
        skill_id: i64,
        judge_agent: &str,
        score: f64,
        rationale: Option<&str>,
    ) -> Result<SkillJudgment> {
        Ok(kleos_lib::skills::add_judgment(
            &self.db,
            skill_id,
            self.user_id,
            judge_agent,
            score,
            rationale,
        )
        .await?)
    }

    /// List all judgments for a skill owned by the session user.
    pub async fn skill_get_judgments(&self, skill_id: i64) -> Result<Vec<SkillJudgment>> {
        Ok(kleos_lib::skills::get_judgments(&self.db, skill_id, self.user_id).await?)
    }

    /// List recent evolutions for skills owned by the session user.
    pub async fn skill_list_recent_evolutions(
        &self,
        since_hours: u32,
        limit: usize,
    ) -> Result<Vec<EvolutionFeedRow>> {
        Ok(
            kleos_lib::skills::list_recent_evolutions(&self.db, self.user_id, since_hours, limit)
                .await?,
        )
    }

    /// Fetch the ancestry chain of a skill owned by the session user.
    pub async fn skill_get_lineage(&self, skill_id: i64) -> Result<Vec<i64>> {
        Ok(kleos_lib::skills::get_lineage(&self.db, skill_id, self.user_id).await?)
    }

    // -- Personality pass-throughs ---------------------------------------------

    /// Store a personality signal for the session user.
    /// `signal_type` names the trait dimension (e.g. "focus", "openness").
    /// `value` is a scalar intensity in [0, 1]. `evidence` and `agent` are optional.
    pub async fn personality_store_signal(
        &self,
        signal_type: &str,
        value: f64,
        evidence: Option<&str>,
        agent: Option<&str>,
    ) -> Result<StoredSignal> {
        Ok(
            kleos_lib::personality::store_signal(
                &self.db,
                signal_type,
                value,
                evidence,
                self.user_id,
                agent,
            )
            .await?,
        )
    }

    /// List the most recent personality signals for the session user (up to `limit`).
    pub async fn personality_list_signals(&self, limit: usize) -> Result<Vec<StoredSignal>> {
        Ok(kleos_lib::personality::list_signals(&self.db, self.user_id, limit).await?)
    }

    /// Retrieve (or lazily compute) the stored personality profile for the session user.
    pub async fn personality_get_profile(&self) -> Result<StoredProfile> {
        Ok(kleos_lib::personality::get_profile(&self.db, self.user_id).await?)
    }

    /// Recompute and persist the personality profile for the session user from stored signals.
    pub async fn personality_update_profile(&self) -> Result<StoredProfile> {
        Ok(kleos_lib::personality::update_profile(&self.db, self.user_id).await?)
    }

    /// Return the personality profile and staleness flag for injection, or `None`
    /// when no profile has been persisted yet for the session user.
    pub async fn personality_get_profile_for_injection(
        &self,
    ) -> Result<Option<(String, bool)>> {
        Ok(kleos_lib::personality::get_profile_for_injection(&self.db, self.user_id).await?)
    }

    /// Detect personality signals in `content` using rule-based extraction.
    /// Pure function -- no database interaction.
    pub fn personality_detect_signals(&self, content: &str) -> Vec<(String, f64)> {
        kleos_lib::personality::detect_signals(content)
    }

    // -- Graph pass-throughs ---------------------------------------------------

    /// Upsert a graph entity owned by the session user. On name+type conflict,
    /// increments the occurrence count and refreshes `last_seen_at`.
    pub async fn graph_create_entity(&self, req: CreateEntityRequest) -> Result<Entity> {
        Ok(kleos_lib::graph::entities::create_entity(&self.db, req, self.user_id).await?)
    }

    /// Fetch one entity by id for the session user.
    pub async fn graph_get_entity(&self, id: i64) -> Result<Entity> {
        Ok(kleos_lib::graph::entities::get_entity(&self.db, id, self.user_id).await?)
    }

    /// List entities for the session user ordered by occurrence count.
    pub async fn graph_list_entities(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<Entity>> {
        Ok(
            kleos_lib::graph::entities::list_entities(&self.db, self.user_id, limit, offset)
                .await?,
        )
    }

    /// Delete a graph entity owned by the session user.
    pub async fn graph_delete_entity(&self, id: i64) -> Result<()> {
        Ok(kleos_lib::graph::entities::delete_entity(&self.db, id, self.user_id).await?)
    }

    /// Search memories linked to a given entity for the session user using FTS.
    pub async fn graph_search_entity_memories(
        &self,
        entity_id: i64,
        query: &str,
        limit: i64,
    ) -> Result<Vec<EntityMemorySearchResult>> {
        Ok(kleos_lib::graph::entities::search_entity_memories(
            &self.db,
            entity_id,
            self.user_id,
            query,
            limit,
        )
        .await?)
    }

    /// Compute PageRank scores over the session user's memory graph.
    /// `damping` is typically 0.85; `max_iterations` is a convergence cap.
    pub async fn graph_compute_pagerank(
        &self,
        damping: f64,
        max_iterations: u32,
    ) -> Result<PageRankResult> {
        Ok(
            kleos_lib::graph::pagerank::compute_pagerank(
                &self.db,
                self.user_id,
                damping,
                max_iterations,
            )
            .await?,
        )
    }

    /// Recompute and persist PageRank scores for the session user.
    pub async fn graph_update_pagerank(&self) -> Result<PageRankUpdateResult> {
        Ok(kleos_lib::graph::pagerank::update_pagerank_scores(&self.db, self.user_id).await?)
    }

    /// Detect Louvain communities in the session user's memory graph.
    /// `max_iterations` caps the Louvain optimization passes (clamped to 100 internally).
    pub async fn graph_detect_communities(&self, max_iterations: u32) -> Result<CommunitiesResult> {
        Ok(
            kleos_lib::graph::communities::detect_communities(&self.db, self.user_id, max_iterations)
                .await?,
        )
    }

    /// Return community membership statistics for the session user.
    pub async fn graph_community_stats(&self) -> Result<Vec<CommunityStats>> {
        Ok(kleos_lib::graph::communities::get_community_stats(&self.db, self.user_id).await?)
    }

    // -- Brain pass-throughs ---------------------------------------------------

    /// List all Hopfield patterns persisted for the session user.
    /// Each pattern corresponds to a stored memory embedding in the brain substrate.
    pub async fn brain_list_patterns(&self) -> Result<Vec<BrainPattern>> {
        Ok(
            kleos_lib::brain::hopfield::pattern::list_patterns(&self.db, self.user_id).await?,
        )
    }

    /// Fetch a single Hopfield pattern by id for the session user.
    pub async fn brain_get_pattern(&self, id: i64) -> Result<BrainPattern> {
        Ok(
            kleos_lib::brain::hopfield::pattern::get_pattern(&self.db, id, self.user_id).await?,
        )
    }

    /// Count the total number of brain edges (associative links) for the session user.
    pub async fn brain_count_edges(&self) -> Result<i64> {
        Ok(kleos_lib::brain::hopfield::edges::count_edges(&self.db, self.user_id).await?)
    }

    /// Fetch edges originating from a given brain pattern for the session user.
    pub async fn brain_get_edges_from(&self, source_id: i64) -> Result<Vec<BrainEdge>> {
        Ok(
            kleos_lib::brain::hopfield::edges::get_edges_from(&self.db, source_id, self.user_id)
                .await?,
        )
    }

    /// Decay all brain edge weights by `rate` for the session user.
    /// Returns the number of edges decayed.
    pub async fn brain_decay_edges(&self, rate: f32) -> Result<usize> {
        Ok(kleos_lib::brain::hopfield::edges::decay_edges(&self.db, self.user_id, rate).await?)
    }

    /// Prune brain edges below `threshold` weight for the session user.
    /// Returns the number of edges removed.
    pub async fn brain_prune_edges(&self, threshold: f32) -> Result<usize> {
        Ok(
            kleos_lib::brain::hopfield::edges::prune_edges(&self.db, self.user_id, threshold)
                .await?,
        )
    }

    // -- Intelligence pass-throughs --------------------------------------------

    /// Return a health report for the session user's memory corpus: counts,
    /// duplicate flags, freshness stats, and importance distribution.
    pub async fn intelligence_memory_health(&self) -> Result<MemoryHealthReport> {
        Ok(kleos_lib::intelligence::health::memory_health(&self.db, self.user_id).await?)
    }

    /// Scan all memory pairs for semantic contradictions and return the set
    /// found. Runs a DB-only FTS comparison; no LLM is required.
    pub async fn intelligence_scan_contradictions(&self) -> Result<Vec<Contradiction>> {
        Ok(
            kleos_lib::intelligence::contradiction::scan_all_contradictions(
                &self.db,
                self.user_id,
            )
            .await?,
        )
    }

    /// List stored reflections for the session user, newest first.
    pub async fn intelligence_list_reflections(&self, limit: usize) -> Result<Vec<Reflection>> {
        Ok(
            kleos_lib::intelligence::reflections::list_reflections(&self.db, self.user_id, limit)
                .await?,
        )
    }

    /// List stored causal chains for the session user, newest first.
    pub async fn intelligence_list_causal_chains(&self, limit: usize) -> Result<Vec<CausalChain>> {
        Ok(
            kleos_lib::intelligence::causal::list_chains(&self.db, self.user_id, limit).await?,
        )
    }

    /// List stored periodic digests for the session user, newest first.
    pub async fn intelligence_list_digests(&self, limit: usize) -> Result<Vec<Digest>> {
        Ok(
            kleos_lib::intelligence::digests::list_digests(&self.db, self.user_id, limit).await?,
        )
    }

    // -- Forge pass-throughs ---------------------------------------------------

    /// Record a new task spec in `forge_specs` and return its JSON descriptor.
    /// Enforces: minimum 2 acceptance criteria, minimum 3 edge cases, and a
    /// valid `task_type` (feature / bugfix / refactor / test / docs / chore).
    #[allow(clippy::too_many_arguments)]
    pub async fn forge_spec_task(
        &self,
        session_id: Option<&str>,
        task_description: String,
        task_type: String,
        acceptance_criteria: Vec<String>,
        interface_contract: String,
        edge_cases: Vec<String>,
        files_to_touch: Option<Vec<String>>,
        dependencies: Option<String>,
    ) -> Result<serde_json::Value> {
        Ok(kleos_lib::forge::spec::spec_task(
            &self.db,
            self.user_id,
            session_id,
            task_description,
            task_type,
            acceptance_criteria,
            interface_contract,
            edge_cases,
            files_to_touch,
            dependencies,
        )
        .await?)
    }

    /// Persist a pre-fix hypothesis to `forge_hypotheses` and return its JSON.
    /// `confidence` must be in [0.0, 1.0]; defaults to 0.7 if `None`.
    pub async fn forge_log_hypothesis(
        &self,
        session_id: Option<&str>,
        bug_description: String,
        hypothesis: String,
        confidence: Option<f64>,
        spec_id: Option<String>,
    ) -> Result<serde_json::Value> {
        Ok(kleos_lib::forge::hypothesis::log_hypothesis(
            &self.db,
            self.user_id,
            session_id,
            bug_description,
            hypothesis,
            confidence,
            spec_id,
        )
        .await?)
    }

    /// Close an open hypothesis with its outcome (`correct` / `incorrect` / `partial`).
    pub async fn forge_log_outcome(
        &self,
        hypothesis_id: String,
        outcome: String,
        notes: Option<String>,
    ) -> Result<serde_json::Value> {
        Ok(kleos_lib::forge::hypothesis::log_outcome(
            &self.db,
            self.user_id,
            hypothesis_id,
            outcome,
            notes,
        )
        .await?)
    }

    /// Record two or more named design alternatives in `forge_approaches` and
    /// return a structured comparison. `chosen_index` marks the selected option.
    pub async fn forge_consider_approaches(
        &self,
        spec_id: Option<String>,
        problem: String,
        approaches: Vec<ApproachItem>,
        chosen_index: Option<usize>,
    ) -> Result<serde_json::Value> {
        Ok(kleos_lib::forge::approaches::consider_approaches(
            &self.db,
            self.user_id,
            spec_id,
            problem,
            approaches,
            chosen_index,
        )
        .await?)
    }

    /// Persist a client-side verification run result to `forge_verifications`.
    #[allow(clippy::too_many_arguments)]
    pub async fn forge_record_verification(
        &self,
        spec_id: Option<String>,
        command: String,
        exit_code: i32,
        success: bool,
        duration_ms: Option<i64>,
        criteria_index: Option<i64>,
        stdout: Option<String>,
        stderr: Option<String>,
    ) -> Result<serde_json::Value> {
        Ok(kleos_lib::forge::verify::record_verification(
            &self.db,
            self.user_id,
            spec_id,
            command,
            exit_code,
            success,
            duration_ms,
            criteria_index,
            stdout,
            stderr,
        )
        .await?)
    }

    /// Record a mid-session discovery in `forge_session_learns`.
    pub async fn forge_session_learn(
        &self,
        discovery: String,
        context: Option<String>,
        tags: Option<Vec<String>>,
        spec_id: Option<String>,
    ) -> Result<serde_json::Value> {
        Ok(kleos_lib::forge::session::session_learn(
            &self.db,
            self.user_id,
            discovery,
            context,
            tags,
            spec_id,
        )
        .await?)
    }

    /// Search past session learnings by keyword. Returns up to `limit` matches.
    pub async fn forge_session_recall(
        &self,
        query: Option<String>,
        limit: Option<usize>,
    ) -> Result<serde_json::Value> {
        Ok(
            kleos_lib::forge::session::session_recall(&self.db, self.user_id, query, limit)
                .await?,
        )
    }
}

/// Tests for cognition storage, retrieval, and facade pass-through behavior.
#[cfg(test)]
mod tests {
    use super::*;
    use kleos_lib::context::types::ContextOptions;
    use kleos_lib::memory::types::{SearchRequest, StoreRequest};

    /// A handoff stored through a tenant-backed session round-trips: the tenant
    /// migration chain (schema_v43) creates the handoffs tables the monolith lite
    /// session lacks, so `handoffs_store` -> `handoffs_latest` succeeds. This is the
    /// proof that closes ledger row 4.
    #[tokio::test]
    async fn tenant_session_handoff_round_trip() {
        let cog = Cognition::open_tenant_memory()
            .await
            .expect("open tenant-backed in-memory cognitive core");

        let stored = cog
            .handoffs_store(HandoffStoreParams {
                project: "henosis".to_string(),
                content: "Row 4: tenant-backed handoffs now work end to end.".to_string(),
                branch: None,
                directory: None,
                agent: None,
                handoff_type: None,
                session_id: None,
                model: None,
                host: None,
                metadata: None,
            })
            .await
            .expect("store handoff against the tenant schema");
        let stored_id = stored.id.expect("store returns a row id");
        assert!(stored_id > 0, "a real handoff id is assigned");

        let latest = cog
            .handoffs_latest(HandoffFilters {
                project: Some("henosis".to_string()),
                ..Default::default()
            })
            .await
            .expect("fetch latest handoff")
            .expect("a handoff exists");
        assert_eq!(latest.id, stored_id);
        assert!(latest.content.contains("tenant-backed handoffs"));
    }

    /// A handoff stored through a path-backed tenant session survives a drop and
    /// reopen: the tenant schema persists to the file and reopen is idempotent.
    #[tokio::test]
    async fn tenant_path_session_handoff_persists_across_reopen() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("tenant.db");
        let db_path = db_path.to_str().expect("utf-8 path");

        let stored_id = {
            let cog = Cognition::open_tenant_path(db_path, DEFAULT_USER_ID)
                .await
                .expect("open path-backed tenant session");
            let stored = cog
                .handoffs_store(HandoffStoreParams {
                    project: "henosis".to_string(),
                    content: "Durable tenant handoff for the reopen proof.".to_string(),
                    branch: None,
                    directory: None,
                    agent: None,
                    handoff_type: None,
                    session_id: None,
                    model: None,
                    host: None,
                    metadata: None,
                })
                .await
                .expect("store handoff");
            stored.id.expect("store returns a row id")
        };

        let reopened = Cognition::open_tenant_path(db_path, DEFAULT_USER_ID)
            .await
            .expect("reopen path-backed tenant session");
        let latest = reopened
            .handoffs_latest(HandoffFilters {
                project: Some("henosis".to_string()),
                ..Default::default()
            })
            .await
            .expect("fetch latest after reopen")
            .expect("the persisted handoff survives");
        assert_eq!(latest.id, stored_id);
    }

    /// Forge pass-throughs smoke test: spec_task, log_hypothesis, and session_learn round-trip.
    #[tokio::test]
    async fn lite_session_forge_round_trip() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open cognitive core");
        // spec_task requires min 2 criteria and min 3 edge cases.
        let spec = cog
            .forge_spec_task(
                None,
                "add a foo widget".into(),
                "feature".into(),
                vec!["foo widget appears".into(), "foo widget saves state".into()],
                "fn foo_widget() -> Widget".into(),
                vec!["empty state".into(), "max items".into(), "concurrent writes".into()],
                None,
                None,
            )
            .await
            .expect("forge_spec_task");
        let spec_id = spec["id"].as_str().expect("spec id").to_string();
        assert!(spec_id.starts_with("spec_"), "id prefix is spec_");

        let hyp = cog
            .forge_log_hypothesis(
                None,
                "widget does not save".into(),
                "state key is wrong".into(),
                Some(0.8),
                Some(spec_id),
            )
            .await
            .expect("forge_log_hypothesis");
        assert!(hyp["id"].as_str().expect("hyp id").starts_with("hyp_"));

        let learned = cog
            .forge_session_learn("db key must use uuid not name".into(), None, None, None)
            .await
            .expect("forge_session_learn");
        assert!(
            learned["id"].as_str().expect("learn id").starts_with("learn_")
        );
    }

    /// Intelligence pass-throughs smoke test: memory_health and contradiction scan succeed on empty DB.
    #[tokio::test]
    async fn lite_session_intelligence_smoke() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open cognitive core");
        let health = cog
            .intelligence_memory_health()
            .await
            .expect("intelligence_memory_health");
        assert_eq!(health.total_memories, 0, "fresh DB has no memories");
        let contradictions = cog
            .intelligence_scan_contradictions()
            .await
            .expect("intelligence_scan_contradictions");
        assert!(contradictions.is_empty(), "fresh DB has no contradictions");
    }

    /// Brain pass-throughs smoke test: a fresh DB has zero edges and zero patterns.
    #[tokio::test]
    async fn lite_session_brain_smoke() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open cognitive core");
        let edge_count = cog.brain_count_edges().await.expect("brain_count_edges");
        assert_eq!(edge_count, 0, "new DB has no brain edges");
        let patterns = cog.brain_list_patterns().await.expect("brain_list_patterns");
        assert!(patterns.is_empty(), "new DB has no brain patterns");
    }

    /// Graph pass-throughs round-trip: create an entity, fetch it by id, then compute pagerank.
    #[tokio::test]
    async fn lite_session_graph_round_trip() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open cognitive core");
        let created = cog
            .graph_create_entity(CreateEntityRequest {
                name: "Henosis".to_string(),
                entity_type: Some("project".to_string()),
                description: Some("The Henosis AI platform.".to_string()),
                aliases: None,
                user_id: None,
                space_id: None,
            })
            .await
            .expect("create graph entity");
        let fetched = cog
            .graph_get_entity(created.id)
            .await
            .expect("get graph entity");
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.name, "Henosis");
        // Pagerank over an empty graph completes without error.
        let pr = cog
            .graph_compute_pagerank(0.85, 10)
            .await
            .expect("compute pagerank");
        let _ = pr;
    }

    /// Personality pass-throughs round-trip: store a signal, then list it back.
    #[tokio::test]
    async fn lite_session_personality_round_trip() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open cognitive core");
        cog.personality_store_signal("focus", 0.8, Some("test evidence"), None)
            .await
            .expect("store personality signal");
        let signals = cog
            .personality_list_signals(10)
            .await
            .expect("list personality signals");
        assert!(!signals.is_empty(), "stored signal is present in list");
        assert_eq!(signals[0].signal_type, "focus");
    }

    /// Skills pass-throughs round-trip: create a skill, then get it back by id.
    #[tokio::test]
    async fn lite_session_skills_round_trip() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open cognitive core");
        let created = cog
            .skill_create(CreateSkillRequest {
                name: "test-skill".to_string(),
                agent: "test-agent".to_string(),
                description: Some("A test skill for the round-trip proof.".to_string()),
                code: "echo hello".to_string(),
                language: Some("bash".to_string()),
                parent_skill_id: None,
                metadata: None,
                user_id: None,
                tags: None,
                tool_deps: None,
                kind: None,
                source_plugin: None,
                source_path: None,
                content_hash: None,
            })
            .await
            .expect("create skill");
        let fetched = cog.skill_get(created.id).await.expect("get skill");
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.name, "test-skill");
    }

    /// The lightweight-session proof: against an in-memory store with NO embedder
    /// and NO server, `memory_store` a record -> `memory_search` (FTS) returns it
    /// -> `assemble_context` succeeds. Exercises only the embedding-free paths so
    /// no ONNX / LanceDB native library is required.
    #[tokio::test]
    async fn lite_session_store_search_context_round_trip() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open in-memory cognitive core");

        // Store a memory (FTS-only path: no embedder, no pre-supplied embedding).
        let stored = cog
            .memory_store(StoreRequest {
                content: "The Henosis cognition facade wraps kleos-lib in process.".to_string(),
                source: "henosis-cognition-test".to_string(),
                ..Default::default()
            })
            .await
            .expect("store memory");
        assert!(stored.created, "first store creates a new memory");
        assert!(stored.id > 0, "a real memory id is assigned");

        // FTS search returns it (no embedding -> lexical path).
        let hits = cog
            .memory_search(SearchRequest {
                query: "cognition facade".to_string(),
                ..Default::default()
            })
            .await
            .expect("search memory");
        assert!(
            hits.iter().any(|h| h.memory.id == stored.id),
            "FTS search surfaces the stored memory: {hits:?}"
        );

        // memory_get round-trips the same row by id.
        let fetched = cog.memory_get(stored.id).await.expect("get memory");
        assert_eq!(fetched.id, stored.id);
        assert!(fetched.content.contains("cognition facade"));

        // assemble_context succeeds with embedder = None (FTS / recency layers).
        let ctx = cog
            .assemble_context(ContextOptions {
                query: "cognition facade".to_string(),
                ..Default::default()
            })
            .await
            .expect("assemble context");
        assert!(ctx.token_budget > 0, "a token budget is resolved");
        assert!(
            ctx.context.contains("cognition facade"),
            "assembled context includes the stored memory: {}",
            ctx.context
        );
    }

    /// A scratchpad pass-through round-trips against the lite in-memory session
    /// (the scratchpad table ships in the monolith schema).
    #[tokio::test]
    async fn lite_session_scratchpad_round_trip() {
        let cog = Cognition::open_in_memory()
            .await
            .expect("open in-memory cognitive core");

        cog.scratchpad_put("sess-1", "tester", "test-model", "phase", "wave2", 60)
            .await
            .expect("scratchpad put");

        let entries = cog
            .scratchpad_list(Some("tester"), None, Some("sess-1"))
            .await
            .expect("scratchpad list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "phase");
        assert_eq!(entries[0].value, "wave2");
    }

    /// The durability proof: a memory stored through a path-backed session is
    /// still searchable after the session is dropped and the same file reopened.
    /// Uses the FTS path (no embedder), so no native ONNX/Lance library is built.
    #[tokio::test]
    async fn path_backed_session_persists_across_reopen() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db_path = dir.path().join("cognition.db");
        let db_path = db_path.to_str().expect("utf-8 path");

        let stored_id = {
            let cog = Cognition::open_path(db_path)
                .await
                .expect("open path-backed session");
            let stored = cog
                .memory_store(StoreRequest {
                    content: "Wave 3 wires the bridge memory onto an in-process cognition store."
                        .to_string(),
                    source: "henosis-cognition-test".to_string(),
                    ..Default::default()
                })
                .await
                .expect("store memory");
            assert!(stored.created, "first store creates a new memory");
            stored.id
            // `cog` drops here: the connection pool closes, leaving only the file.
        };

        // Reopen the same file: migrations are idempotent and the row survives.
        let reopened = Cognition::open_path(db_path)
            .await
            .expect("reopen path-backed session");
        let hits = reopened
            .memory_search(SearchRequest {
                query: "bridge memory cognition".to_string(),
                ..Default::default()
            })
            .await
            .expect("search after reopen");
        assert!(
            hits.iter().any(|h| h.memory.id == stored_id),
            "the persisted memory survives a reopen: {hits:?}"
        );
    }
}
