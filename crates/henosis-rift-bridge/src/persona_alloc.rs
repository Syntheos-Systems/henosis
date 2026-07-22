//! Room-level Frameshift persona allocation.
//!
//! Assigns thread-stable personas to the agents in a room: ranks candidates via
//! `frameshift-orchestrator`, avoids over-assigning the same persona, and (when
//! enabled) reserves one challenger slot forced to a contrarian stance. Personas
//! are selected once per discussion thread and held for its duration.

use std::collections::HashSet;
use std::path::PathBuf;

use frameshift_orchestrator::feedback::Preferences;
use frameshift_orchestrator::policy::PolicyWeights;
use frameshift_orchestrator::run::{select, SelectionInputs};

use crate::error::BridgeError;
use crate::types::AgentId;

/// A persona assigned to one agent for a thread.
#[derive(Debug, Clone)]
pub struct PersonaAssignment {
    /// The agent this persona is assigned to.
    pub agent_id: AgentId,
    /// The selected persona's name.
    pub persona_name: String,
    /// Whether this agent occupies the challenger (contrarian) slot.
    pub is_challenger: bool,
    /// The persona's interest terms, used downstream for relevance scoring.
    pub interests: Vec<String>,
}

/// Room-level, thread-stable persona allocator over a Frameshift library.
pub struct PersonaAllocator {
    /// Frameshift persona library (catalog) root.
    library_path: PathBuf,
    /// Maximum agents allowed to hold the same persona in one thread.
    max_same_persona: usize,
    /// Whether to reserve one challenger slot.
    challenger_slot: bool,
}

/// A ranked persona candidate paired with its derived interest terms.
///
/// This is the unit the pure allocation helper operates on, decoupling the
/// collision/challenger/determinism logic from the orchestrator call so it can
/// be unit-tested without building a real catalog.
#[derive(Debug, Clone)]
struct RankedPersona {
    /// Persona name as returned by the orchestrator ranking.
    name: String,
    /// Interest terms derived for this persona (see `derive_interests`).
    interests: Vec<String>,
}

/// One allocation decision for a single agent (persona + flags), produced by the
/// pure helper before being zipped back onto concrete `AgentId`s.
#[derive(Debug, Clone, PartialEq)]
struct ChosenPersona {
    /// The persona name assigned to this agent.
    name: String,
    /// Whether this agent is the challenger.
    is_challenger: bool,
    /// Interest terms carried over from the ranked candidate.
    interests: Vec<String>,
}

/// Implements thread-stable persona allocation with collision avoidance.
impl PersonaAllocator {
    /// Construct an allocator over a Frameshift library with the given policy.
    pub fn new(library_path: PathBuf, max_same_persona: usize, challenger_slot: bool) -> Self {
        Self {
            library_path,
            max_same_persona,
            challenger_slot,
        }
    }

    /// Allocate thread-stable personas to `agents` for `thread_id`.
    ///
    /// Ranks candidates with `frameshift-orchestrator::select`, refuses to assign
    /// a persona already held by `max_same_persona` agents this thread, and (if
    /// `challenger_slot`) forces exactly one agent into a contrarian persona.
    /// Stable: the same `(thread_id, agents, task_hint)` yields the same result.
    ///
    /// `interests` for each assignment are derived by tokenizing the persona's
    /// name plus the orchestrator's rationale (lowercase, split on non-alphanumeric,
    /// dropping stopwords and short tokens). The persona's full Frameshift
    /// definition is not re-parsed here -- `select` does not surface it, and the
    /// rationale already reflects why the persona matched the task.
    pub fn allocate(
        &self,
        thread_id: &str,
        agents: &[AgentId],
        task_hint: Option<&str>,
    ) -> Result<Vec<PersonaAssignment>, BridgeError> {
        // Nothing to assign -- short-circuit before touching the catalog.
        if agents.is_empty() {
            return Ok(Vec::new());
        }

        // A cap of zero can never be satisfied; treat it as misconfiguration.
        if self.max_same_persona == 0 {
            return Err(BridgeError::Config(
                "max_same_persona must be at least 1".to_string(),
            ));
        }

        // Rank personas once. The library is both the catalog to index and the
        // project root used for context sensing (a stable, existing directory),
        // keeping the call deterministic for fixed inputs.
        let inputs = SelectionInputs {
            project_root: self.library_path.as_path(),
            task_hint,
            source_dirs: Vec::new(),
            catalog_root: Some(self.library_path.clone()),
            prefs: Preferences::default(),
            weights: PolicyWeights::default(),
        };

        let ranked = select(&inputs)
            .map_err(|e| BridgeError::Config(format!("persona ranking failed: {e}")))?;

        if ranked.is_empty() {
            return Err(BridgeError::Config(format!(
                "no personas found in library {}",
                self.library_path.display()
            )));
        }

        // Pair each ranked persona with its derived interest terms.
        let candidates: Vec<RankedPersona> = ranked
            .iter()
            .map(|s| RankedPersona {
                name: s.persona.clone(),
                interests: derive_interests(&s.persona, &s.rationale),
            })
            .collect();

        // Run the pure allocation logic, seeded deterministically by thread_id.
        let chosen = allocate_from_ranked(
            &candidates,
            agents.len(),
            self.max_same_persona,
            self.challenger_slot,
            fnv1a64(thread_id.as_bytes()),
        );

        // Zip persona decisions back onto concrete agent ids.
        let assignments = agents
            .iter()
            .zip(chosen)
            .map(|(agent_id, c)| PersonaAssignment {
                agent_id: *agent_id,
                persona_name: c.name,
                is_challenger: c.is_challenger,
                interests: c.interests,
            })
            .collect();

        Ok(assignments)
    }
}

/// Pure persona-to-agent assignment over a pre-ranked candidate list.
///
/// Walks `ranked` (highest score first) assigning a persona per agent while
/// enforcing the collision cap: a persona already held by `max_same_persona`
/// agents is skipped. When every candidate is at the cap (more agents than the
/// cap can absorb), it degrades gracefully to the least-used persona, breaking
/// ties by `(seed, name)` hash then rank order so the result stays deterministic.
///
/// When `challenger_slot` is set, the last agent is forced to the highest-ranked
/// persona NOT chosen by the consensus group and flagged `is_challenger`. The cap
/// does not apply to the challenger -- its whole purpose is to differ. If no
/// distinct persona exists, the challenger falls back to the top-ranked persona,
/// still flagged.
///
/// Deterministic: identical arguments always yield identical output. `ranked`
/// must be non-empty (callers guarantee this).
fn allocate_from_ranked(
    ranked: &[RankedPersona],
    n_agents: usize,
    max_same_persona: usize,
    challenger_slot: bool,
    seed: u64,
) -> Vec<ChosenPersona> {
    debug_assert!(
        !ranked.is_empty(),
        "ranked candidate list must be non-empty"
    );

    // Per-candidate assignment counts, parallel to `ranked`.
    let mut counts = vec![0usize; ranked.len()];

    // The last agent is the challenger when enabled; the rest form consensus.
    let consensus_count = if challenger_slot {
        n_agents.saturating_sub(1)
    } else {
        n_agents
    };

    let mut chosen: Vec<ChosenPersona> = Vec::with_capacity(n_agents);

    // --- Consensus group: cap-aware greedy walk down the ranking. ---
    for _ in 0..consensus_count {
        let idx = pick_consensus(ranked, &counts, max_same_persona, seed);
        counts[idx] += 1;
        chosen.push(ChosenPersona {
            name: ranked[idx].name.clone(),
            is_challenger: false,
            interests: ranked[idx].interests.clone(),
        });
    }

    // --- Challenger: highest-ranked persona not in the consensus set. ---
    if challenger_slot {
        let consensus_names: HashSet<&str> = chosen.iter().map(|c| c.name.as_str()).collect();
        let idx = ranked
            .iter()
            .position(|p| !consensus_names.contains(p.name.as_str()))
            // Every candidate already taken by consensus -> fall back to the top.
            .unwrap_or(0);
        chosen.push(ChosenPersona {
            name: ranked[idx].name.clone(),
            is_challenger: true,
            interests: ranked[idx].interests.clone(),
        });
    }

    chosen
}

/// Pick the index of the next consensus persona.
///
/// Prefers the highest-ranked persona still under the collision cap. If none is
/// under the cap, returns the least-used persona, breaking ties deterministically
/// by `(seed, name)` hash and finally by rank order.
fn pick_consensus(
    ranked: &[RankedPersona],
    counts: &[usize],
    max_same_persona: usize,
    seed: u64,
) -> usize {
    // First choice: top-ranked persona that has not hit the cap.
    if let Some(idx) = (0..ranked.len()).find(|&i| counts[i] < max_same_persona) {
        return idx;
    }

    // Saturated: fall back to the least-used persona, deterministic tie-break.
    let mut best = 0usize;
    for i in 1..ranked.len() {
        if better_fallback(ranked, counts, seed, i, best) {
            best = i;
        }
    }
    best
}

/// Return true when candidate `i` is a strictly better fallback than `best`:
/// fewer assignments, then lower `(seed, name)` hash, then lower rank index.
fn better_fallback(
    ranked: &[RankedPersona],
    counts: &[usize],
    seed: u64,
    i: usize,
    best: usize,
) -> bool {
    match counts[i].cmp(&counts[best]) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        std::cmp::Ordering::Equal => {
            let hi = tie_key(seed, &ranked[i].name);
            let hb = tie_key(seed, &ranked[best].name);
            // Lower hash wins; rank index already favors `best` (lower i) on a tie.
            hi < hb
        }
    }
}

/// Deterministic per-(seed, name) tie-break key.
fn tie_key(seed: u64, name: &str) -> u64 {
    let mut h = seed;
    // Mix the seed into the name hash so different threads break ties differently.
    h ^= fnv1a64(name.as_bytes());
    h
}

/// 64-bit FNV-1a hash. Deterministic across runs (no random seeding), unlike
/// `std`'s `RandomState`, which is required for thread-stable allocation.
fn fnv1a64(bytes: &[u8]) -> u64 {
    // FNV-1a offset basis and prime.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Derive interest terms for a persona from its name and the ranking rationale.
///
/// Lowercases, splits on non-alphanumeric boundaries, drops stopwords and tokens
/// shorter than three characters, and de-duplicates while preserving first-seen
/// order. The persona name's tokens come first so a persona always carries its
/// own identity terms even when the rationale is sparse.
fn derive_interests(name: &str, rationale: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for source in [name, rationale] {
        for raw in source.split(|c: char| !c.is_alphanumeric()) {
            if raw.is_empty() {
                continue;
            }
            let token = raw.to_lowercase();
            if token.len() < 3 || is_stopword(&token) {
                continue;
            }
            if seen.insert(token.clone()) {
                out.push(token);
            }
        }
    }

    out
}

/// Whether a lowercased token is a common stopword and should be dropped from
/// derived interests.
fn is_stopword(token: &str) -> bool {
    // Small, fixed stopword set covering frequent English filler plus rationale
    // boilerplate ("score", "persona", "match") that carries no interest signal.
    const STOPWORDS: &[&str] = &[
        "the",
        "and",
        "for",
        "with",
        "this",
        "that",
        "from",
        "into",
        "than",
        "then",
        "are",
        "was",
        "were",
        "has",
        "have",
        "had",
        "not",
        "but",
        "its",
        "his",
        "her",
        "you",
        "your",
        "our",
        "out",
        "all",
        "any",
        "can",
        "will",
        "would",
        "should",
        "which",
        "while",
        "when",
        "where",
        "what",
        "who",
        "how",
        "why",
        "via",
        "per",
        "score",
        "scored",
        "scores",
        "persona",
        "personas",
        "match",
        "matched",
        "matches",
        "matching",
        "rank",
        "ranked",
        "task",
        "rationale",
        "best",
        "top",
    ];
    STOPWORDS.contains(&token)
}

/// Unit tests for deterministic persona allocation and collision limits.
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a ranked candidate list from `(name, interest...)` tuples.
    fn ranked(names: &[&str]) -> Vec<RankedPersona> {
        names
            .iter()
            .map(|n| RankedPersona {
                name: (*n).to_string(),
                interests: vec![format!("{n}-interest")],
            })
            .collect()
    }

    /// With no cap pressure, agents fill straight down the ranking.
    #[test]
    fn fills_top_ranked_first() {
        let r = ranked(&["alpha", "beta", "gamma"]);
        let out = allocate_from_ranked(&r, 3, 1, false, 1);
        assert_eq!(
            out.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        assert!(out.iter().all(|c| !c.is_challenger));
    }

    /// No persona is assigned more than `max_same_persona` times while distinct
    /// candidates under the cap remain.
    #[test]
    fn respects_collision_cap() {
        let r = ranked(&["alpha", "beta", "gamma", "delta"]);
        // 4 agents, cap 2: top two personas each used at most twice.
        let out = allocate_from_ranked(&r, 4, 2, false, 7);
        for name in ["alpha", "beta", "gamma", "delta"] {
            let used = out.iter().filter(|c| c.name == name).count();
            assert!(used <= 2, "{name} used {used} times, cap was 2");
        }
        // Expect the cap to actually spread choices: alpha x2, beta x2.
        assert_eq!(out.iter().filter(|c| c.name == "alpha").count(), 2);
        assert_eq!(out.iter().filter(|c| c.name == "beta").count(), 2);
    }

    /// When agents outnumber what the cap can absorb, allocation degrades to the
    /// least-used persona rather than panicking or exceeding bounds silently.
    #[test]
    fn degrades_when_cap_cannot_be_met() {
        let r = ranked(&["alpha", "beta"]);
        // 5 agents, 2 personas, cap 1: unsatisfiable, must still assign all 5.
        let out = allocate_from_ranked(&r, 5, 1, false, 3);
        assert_eq!(out.len(), 5);
        // Counts stay as balanced as possible: 3/2 or 2/3.
        let a = out.iter().filter(|c| c.name == "alpha").count();
        let b = out.iter().filter(|c| c.name == "beta").count();
        assert_eq!(a + b, 5);
        assert!(
            (a as i64 - b as i64).abs() <= 1,
            "fallback should balance load"
        );
    }

    /// The challenger slot is the last agent and takes the top-ranked persona not
    /// used by the consensus group.
    #[test]
    fn challenger_slot_assigned() {
        let r = ranked(&["alpha", "beta", "gamma"]);
        let out = allocate_from_ranked(&r, 3, 3, true, 11);
        assert_eq!(out.len(), 3);
        // First two are consensus (alpha, alpha under cap 3), last is challenger.
        assert!(!out[0].is_challenger);
        assert!(!out[1].is_challenger);
        assert!(out[2].is_challenger, "last agent must be challenger");
        // Exactly one challenger.
        assert_eq!(out.iter().filter(|c| c.is_challenger).count(), 1);
        // Challenger persona differs from the consensus persona.
        let consensus: HashSet<&str> = out[..2].iter().map(|c| c.name.as_str()).collect();
        assert!(
            !consensus.contains(out[2].name.as_str()),
            "challenger must differ from consensus"
        );
    }

    /// A lone agent with the challenger slot enabled becomes the challenger and
    /// takes the top-ranked persona (no consensus to differ from).
    #[test]
    fn single_agent_challenger() {
        let r = ranked(&["alpha", "beta"]);
        let out = allocate_from_ranked(&r, 1, 2, true, 5);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_challenger);
        assert_eq!(out[0].name, "alpha");
    }

    /// When only one persona exists, the challenger falls back to it (still flagged).
    #[test]
    fn challenger_falls_back_when_no_distinct_persona() {
        let r = ranked(&["solo"]);
        let out = allocate_from_ranked(&r, 2, 5, true, 9);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].name, "solo");
        assert!(out[1].is_challenger);
    }

    /// Same inputs (including seed) always produce the same assignment.
    #[test]
    fn deterministic_for_same_inputs() {
        let r = ranked(&["alpha", "beta", "gamma", "delta"]);
        let a = allocate_from_ranked(&r, 6, 1, true, 0xdead_beef);
        let b = allocate_from_ranked(&r, 6, 1, true, 0xdead_beef);
        assert_eq!(a, b, "allocation must be deterministic");
    }

    /// The seed (derived from thread_id) influences fallback tie-breaking, so
    /// different threads can produce different -- but each internally stable --
    /// load-balanced assignments.
    #[test]
    fn seed_can_affect_fallback_tiebreak() {
        // Many agents, many equally-capped personas forces fallback tie-breaks.
        let r = ranked(&["a", "b", "c", "d", "e"]);
        let s1 = allocate_from_ranked(&r, 10, 1, false, 1);
        let s2 = allocate_from_ranked(&r, 10, 1, false, 999_999);
        // Both are valid and complete...
        assert_eq!(s1.len(), 10);
        assert_eq!(s2.len(), 10);
        // ...and each is internally deterministic.
        assert_eq!(s1, allocate_from_ranked(&r, 10, 1, false, 1));
        assert_eq!(s2, allocate_from_ranked(&r, 10, 1, false, 999_999));
    }

    /// Interest derivation tokenizes name + rationale, drops stopwords/short tokens,
    /// dedupes, and keeps name tokens first.
    #[test]
    fn derive_interests_tokenizes_and_filters() {
        let got = derive_interests(
            "rust-engineer",
            "Best match for the task: cargo clippy and ownership; rust rust.",
        );
        // Name tokens lead.
        assert_eq!(got[0], "rust");
        assert!(got.contains(&"engineer".to_string()));
        assert!(got.contains(&"cargo".to_string()));
        assert!(got.contains(&"clippy".to_string()));
        assert!(got.contains(&"ownership".to_string()));
        // Stopwords dropped.
        assert!(!got.contains(&"the".to_string()));
        assert!(!got.contains(&"and".to_string()));
        assert!(!got.contains(&"best".to_string()));
        assert!(!got.contains(&"match".to_string()));
        assert!(!got.contains(&"task".to_string()));
        // Short tokens dropped (none of len < 3).
        assert!(got.iter().all(|t| t.len() >= 3));
        // Deduped: "rust" appears once despite repetition.
        assert_eq!(got.iter().filter(|t| *t == "rust").count(), 1);
    }

    /// FNV-1a is stable and order-sensitive.
    #[test]
    fn fnv_is_stable_and_distinct() {
        assert_eq!(fnv1a64(b"thread-42"), fnv1a64(b"thread-42"));
        assert_ne!(fnv1a64(b"thread-42"), fnv1a64(b"thread-43"));
    }
}
