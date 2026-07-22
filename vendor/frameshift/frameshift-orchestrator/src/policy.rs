//! Scoring policy: rank personas against a context signal.

use crate::context::ContextSignal;
use crate::embed::{semantic_similarity, Embedder};
use crate::feedback::Preferences;
use crate::index::{PersonaIndex, PersonaProfile};

/// Score added per project-dependency token (from `ContextSignal::context_tokens`)
/// that matches a persona keyword.
const CONTEXT_TOKEN_HIT: f32 = 0.04;

/// Maximum additive bonus contributed by the semantic-similarity channel.
///
/// The cosine similarity in `[0, 1]` is scaled by this weight and added on top
/// of the weighted blend, mirroring the context bonus: it can lift a persona
/// whose meaning matches the task but never penalizes others or redistributes
/// the primary weights. Contributes `0.0` when no embedder is supplied.
const SEMANTIC_WEIGHT: f32 = 0.15;

/// Maximum total bonus contributed by dependency-token matches. Capped so a
/// dependency-heavy project cannot let framework matching dominate the language,
/// lexical, and intent signals.
const CONTEXT_TOKEN_CAP: f32 = 0.12;

/// Weights controlling the relative contribution of each scoring component.
///
/// Values should sum to 1.0 for predictable score magnitudes, but this is
/// not enforced; they are applied independently and then blended.
#[derive(Debug, Clone)]
pub struct PolicyWeights {
    /// Weight for language overlap scoring.
    pub language: f32,
    /// Weight for lexical (task-token vs. keyword) overlap scoring.
    pub lexical: f32,
    /// Weight for intent alignment scoring.
    pub intent: f32,
    /// Weight for capability heuristic scoring.
    pub capability: f32,
}

/// Supplies balanced default weights for selection scoring.
impl Default for PolicyWeights {
    /// Returns the default weights: language 0.3, lexical 0.25, intent 0.3, capability 0.15.
    fn default() -> Self {
        PolicyWeights {
            language: 0.3,
            lexical: 0.25,
            intent: 0.3,
            capability: 0.15,
        }
    }
}

/// The raw, per-component scores that contributed to a final blended score.
#[derive(Debug, Clone)]
pub struct ScoreComponents {
    /// Language overlap component (0..1).
    pub language: f32,
    /// Lexical overlap component (0..1), after any anti-keyword penalty.
    pub lexical: f32,
    /// Intent alignment component (0..1).
    pub intent: f32,
    /// Capability heuristic component (0..1).
    pub capability: f32,
    /// Project-signal match bonus (0..[`CONTEXT_TOKEN_CAP`]) from dependency and
    /// build-framework matches, added on top of the weighted blend rather than
    /// diluting the lexical channel.
    pub context: f32,
    /// Semantic-similarity bonus (0..[`SEMANTIC_WEIGHT`]) from cosine distance
    /// between the task text and the persona text, when an embedder is supplied.
    /// `0.0` when no embedder is in use (the default), so it is inert by default.
    pub semantic: f32,
}

/// A scored persona with rationale and confidence information.
#[derive(Debug, Clone)]
pub struct Scored {
    /// The persona's canonical name.
    pub persona: String,

    /// Blended score in [0.0, 1.0] after applying weights and preference bias.
    pub score: f32,

    /// Confidence in [0.0, 1.0], derived from the absolute score and the margin
    /// over the second-ranked candidate. Only meaningful relative to the ranking.
    pub confidence: f32,

    /// Human-readable explanation of why this score was assigned.
    pub rationale: String,

    /// Per-component raw scores before blending.
    pub components: ScoreComponents,
}

/// Rank all personas in `index` for the given `ctx` without a semantic embedder.
///
/// Thin wrapper over [`rank_with_embedder`] with the embedder disabled. This is
/// the default entry point; the semantic channel contributes `0.0`, so results
/// are identical to the scorer before semantic matching was added.
pub fn rank(
    ctx: &ContextSignal,
    index: &PersonaIndex,
    weights: &PolicyWeights,
    prefs: &Preferences,
) -> Vec<Scored> {
    rank_with_embedder(ctx, index, weights, prefs, None)
}

/// Build the text used to embed a persona for semantic matching.
///
/// Concatenates the persona's optional description with its keyword set so the
/// embedder sees both the prose summary and the discriminating terms. Keyword
/// order is unspecified (it is a set), which suits bag-of-words style embedders.
fn persona_text(profile: &PersonaProfile) -> String {
    let mut text = profile.description.clone().unwrap_or_default();
    for kw in &profile.keywords {
        text.push(' ');
        text.push_str(kw);
    }
    text
}

/// Rank all personas in `index` for the given `ctx`, optionally using `embedder`.
///
/// Scoring:
/// - Language score: sum of `ctx.languages` weights for languages present in
///   the persona profile, normalized to [0.0, 1.0].
/// - Lexical score: fraction of `ctx.task_tokens` that appear in persona
///   keywords; 0.0 if there are no task tokens.
/// - Capability score: small bonus if the persona has no required tools and
///   no network egress (i.e. "safe" / simple persona).
/// - Semantic score: when `embedder` is `Some` and the task has tokens, the
///   cosine similarity between the task text and each persona's text, scaled by
///   [`SEMANTIC_WEIGHT`] and added to the blend. `None` contributes `0.0`, so
///   the default path has no semantic component and no behavior change.
/// - Per-persona preference bias from `prefs` is added after blending and
///   clamped to [0.0, 1.0].
///
/// Returns results sorted descending by blended score. Confidence is computed
/// after sorting based on the top-vs-runner-up gap and absolute score.
pub fn rank_with_embedder(
    ctx: &ContextSignal,
    index: &PersonaIndex,
    weights: &PolicyWeights,
    prefs: &Preferences,
    embedder: Option<&dyn Embedder>,
) -> Vec<Scored> {
    // Precompute IDF for each task token: tokens that appear in fewer persona
    // keyword sets are weighted higher (rare tokens are more discriminating).
    // idf[tok] = log2(n_personas / (df + 1) + 1), clamped to [0, ∞).
    let n_personas = index.profiles.len().max(1) as f32;
    let idf_weights: std::collections::HashMap<&str, f32> = if ctx.task_tokens.is_empty() {
        std::collections::HashMap::new()
    } else {
        ctx.task_tokens
            .iter()
            .map(|tok| {
                let df = index
                    .profiles
                    .iter()
                    .filter(|p| p.keywords.contains(tok))
                    .count() as f32;
                let idf = (n_personas / (df + 1.0) + 1.0).log2();
                (tok.as_str(), idf)
            })
            .collect()
    };
    // Maximum possible IDF sum (all tokens have df=0, i.e., unique per persona).
    let max_idf_sum: f32 = idf_weights.values().sum::<f32>().max(f32::EPSILON);

    let mut scored: Vec<Scored> = index
        .profiles
        .iter()
        .map(|profile| {
            // Language score: IDF-style precision -- reward personas whose language
            // set PRECISELY covers the context languages rather than broadly.
            // matching_langs / persona_lang_count gives higher scores to specialist
            // personas (fewer languages, tighter match) than generalist ones.
            // Blended 50/50 with the recall-side (lang_sum / ctx.lang_count) to
            // balance precision and recall.
            let matching_lang_sum: f32 = ctx
                .languages
                .iter()
                .filter(|(lang, _)| profile.languages.contains(*lang))
                .map(|(_, weight)| weight)
                .sum();
            let persona_lang_count = profile.languages.len().max(1) as f32;
            let lang_score = if ctx.languages.is_empty() {
                0.0
            } else {
                // Recall: fraction of context languages covered by this persona.
                let recall = (matching_lang_sum / ctx.languages.len() as f32).min(1.0);
                // Precision: fraction of persona's languages that are in the context.
                let precision = (matching_lang_sum / persona_lang_count).min(1.0);
                // F1-style blend: harmonic mean of precision and recall.
                if precision + recall > 0.0 {
                    2.0 * precision * recall / (precision + recall)
                } else {
                    0.0
                }
            };

            // Lexical score: IDF-weighted sum of task token hits normalized to [0.0, 1.0].
            // Rare task tokens (appearing in fewer personas) contribute more weight than
            // common tokens, rewarding specialist personas over generalist ones.
            let lex_score = if ctx.task_tokens.is_empty() {
                0.0
            } else {
                let hit_idf_sum: f32 = ctx
                    .task_tokens
                    .iter()
                    .filter(|tok| profile.keywords.contains(*tok))
                    .map(|tok| idf_weights.get(tok.as_str()).copied().unwrap_or(0.0))
                    .sum();
                (hit_idf_sum / max_idf_sum).min(1.0)
            };

            // Intent score: how well the persona's declared intents match the task intent.
            let intent_score = if let Some(task_intent) = ctx.inferred_intent {
                if profile.primary_intents.is_empty() {
                    0.0
                } else {
                    profile
                        .primary_intents
                        .iter()
                        .map(|pi| crate::intent::relatedness(*pi, task_intent))
                        .fold(0.0_f32, f32::max)
                }
            } else {
                0.0
            };

            // Anti-keyword penalty: penalize lexical score if task tokens match anti-keywords.
            let anti_hit_count = if profile.anti_keywords.is_empty() {
                0
            } else {
                ctx.task_tokens
                    .iter()
                    .filter(|t| {
                        profile
                            .anti_keywords
                            .iter()
                            .any(|ak| ak.eq_ignore_ascii_case(t.as_str()))
                    })
                    .count()
            };
            let lex_score = if anti_hit_count > 0 && !ctx.task_tokens.is_empty() {
                let penalty = (anti_hit_count as f32 / ctx.task_tokens.len() as f32) * 0.5;
                (lex_score - penalty).max(0.0)
            } else {
                lex_score
            };

            // Capability score: prefer personas with no required tools and no network egress.
            let cap_score = if profile.required_tools.is_empty() && !profile.network_egress {
                1.0
            } else if profile.required_tools.is_empty() || !profile.network_egress {
                0.5
            } else {
                0.0
            };

            // Context-signal bonus: a small, capped reward for personas whose
            // keywords match a project signal -- either a declared dependency
            // (`context_tokens`) or a detected build framework (`frameworks`,
            // e.g. cargo/npm/go/python). Kept separate from the lexical IDF
            // channel so that signals matching no persona cannot dilute
            // task-token scoring.
            let context_hits = ctx
                .context_tokens
                .iter()
                .chain(ctx.frameworks.iter())
                .filter(|t| profile.keywords.contains(*t))
                .count();
            let context_score = (context_hits as f32 * CONTEXT_TOKEN_HIT).min(CONTEXT_TOKEN_CAP);

            // Semantic-similarity bonus: when an embedder is supplied, reward
            // personas whose text is close in MEANING to the task -- catching
            // matches the exact-token lexical channel misses. Additive and capped
            // by SEMANTIC_WEIGHT, like the context bonus. The task text is the
            // normalized task tokens; Phase 2 may embed the raw task hint instead.
            // No embedder (the default) yields 0.0, so there is no regression.
            let semantic_score = match embedder {
                Some(emb) if !ctx.task_tokens.is_empty() => {
                    let task_text = ctx.task_tokens.join(" ");
                    let sim = semantic_similarity(emb, &task_text, &persona_text(profile));
                    SEMANTIC_WEIGHT * sim
                }
                _ => 0.0,
            };

            // Blended score. The context and semantic bonuses are additive (not
            // weighted): they can only raise a persona that matches the project
            // or the task meaning, never lower others or redistribute the weights.
            let blended = weights.language * lang_score
                + weights.lexical * lex_score
                + weights.intent * intent_score
                + weights.capability * cap_score
                + context_score
                + semantic_score;

            // Apply preference bias and clamp.
            // Use intent-aware lookup when the context carries an inferred intent;
            // days_since_override is 0 here -- the daemon will supply the real value.
            let bias = if let Some(intent) = ctx.inferred_intent {
                prefs.effective_bias_for(&profile.name, Some(intent), 0)
            } else {
                prefs.bias_for(&profile.name)
            };
            let final_score = (blended + bias).clamp(0.0, 1.0);

            // Build rationale string.
            let mut rationale_parts: Vec<String> = Vec::new();
            if lang_score > 0.0 {
                let matched_langs: Vec<&str> = ctx
                    .languages
                    .keys()
                    .filter(|l| profile.languages.contains(*l))
                    .map(|l| l.as_str())
                    .collect();
                rationale_parts.push(format!(
                    "languages {{{}}}: lang_score={:.2}",
                    matched_langs.join(","),
                    lang_score
                ));
            }
            if lex_score > 0.0 {
                let hit_tokens: Vec<&str> = ctx
                    .task_tokens
                    .iter()
                    .filter(|tok| profile.keywords.contains(*tok))
                    .map(|t| t.as_str())
                    .collect();
                rationale_parts.push(format!(
                    "task tokens [{}] hit persona keywords: lex_score={:.2}",
                    hit_tokens.join(","),
                    lex_score
                ));
            }
            if intent_score > 0.0 {
                rationale_parts.push(format!("intent_score={intent_score:.2}"));
            }
            if cap_score > 0.0 {
                rationale_parts.push(format!("cap_score={cap_score:.2}"));
            }
            if context_score > 0.0 {
                let hit_signals: Vec<&str> = ctx
                    .context_tokens
                    .iter()
                    .chain(ctx.frameworks.iter())
                    .filter(|t| profile.keywords.contains(*t))
                    .map(|t| t.as_str())
                    .collect();
                rationale_parts.push(format!(
                    "project signals [{}] match: context_score={:.2}",
                    hit_signals.join(","),
                    context_score
                ));
            }
            if semantic_score > 0.0 {
                rationale_parts.push(format!("semantic_score={semantic_score:.2}"));
            }
            if bias.abs() > f32::EPSILON {
                rationale_parts.push(format!("pref_bias={bias:.3}"));
            }
            let rationale = if rationale_parts.is_empty() {
                format!("{}: no signal matched", profile.name)
            } else {
                format!(
                    "{} {:.2}: {}",
                    profile.name,
                    final_score,
                    rationale_parts.join("; ")
                )
            };

            let components = ScoreComponents {
                language: lang_score,
                lexical: lex_score,
                intent: intent_score,
                capability: cap_score,
                context: context_score,
                semantic: semantic_score,
            };

            Scored {
                persona: profile.name.clone(),
                score: final_score,
                confidence: 0.0, // filled in after sort
                rationale,
                components,
            }
        })
        .collect();

    // Sort descending by score.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Compute confidence for the top entry based on absolute score and margin.
    if let Some(top) = scored.first_mut() {
        top.confidence = top.score; // absolute component
    }
    if scored.len() >= 2 {
        let top_score = scored[0].score;
        let second_score = scored[1].score;
        let margin = (top_score - second_score).clamp(0.0, 1.0);
        // Blend absolute score and margin equally for confidence.
        scored[0].confidence = (top_score * 0.5 + margin * 0.5).clamp(0.0, 1.0);
    }

    scored
}

#[cfg(test)]
/// Persona ranking, confidence, and weighting tests.
mod tests {
    use super::*;
    use crate::index::{PersonaIndex, PersonaProfile};
    use std::collections::BTreeSet;

    /// Build a minimal PersonaProfile with given languages and keywords.
    fn make_profile(name: &str, languages: &[&str], keywords: &[&str]) -> PersonaProfile {
        PersonaProfile {
            name: name.to_string(),
            description: None,
            languages: languages
                .iter()
                .map(|l| l.to_string())
                .collect::<BTreeSet<_>>(),
            keywords: keywords.iter().map(|k| k.to_string()).collect(),
            required_tools: vec![],
            network_egress: false,
            primary_intents: vec![],
            anti_keywords: vec![],
        }
    }

    /// Build a ContextSignal with given languages and task tokens.
    fn make_ctx(languages: &[(&str, f32)], task_tokens: &[&str]) -> ContextSignal {
        use std::collections::BTreeMap;
        ContextSignal {
            project_name: "test".to_string(),
            languages: languages
                .iter()
                .map(|(l, w)| (l.to_string(), *w))
                .collect::<BTreeMap<_, _>>(),
            frameworks: vec![],
            task_tokens: task_tokens.iter().map(|t| t.to_string()).collect(),
            context_tokens: vec![],
            inferred_intent: None,
        }
    }

    /// A rust-heavy context ranks a rust persona above an unrelated one.
    #[test]
    fn rust_context_ranks_rust_persona_first() {
        let rust_profile = make_profile(
            "rust-expert",
            &["rust"],
            &["rust", "cargo", "clippy", "memory"],
        );
        let web_profile = make_profile(
            "web-designer",
            &["javascript", "typescript"],
            &["react", "css", "html", "frontend"],
        );
        let index = PersonaIndex {
            profiles: vec![rust_profile, web_profile],
        };

        let ctx = make_ctx(&[("rust", 1.0), ("toml", 0.2)], &["clippy", "lint"]);
        let weights = PolicyWeights::default();
        let prefs = Preferences::new();
        let ranked = rank(&ctx, &index, &weights, &prefs);

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].persona, "rust-expert");
        assert!(ranked[0].score > ranked[1].score);
        assert!(!ranked[0].rationale.is_empty());
    }

    /// No-task-tokens: lexical score is zero for all; language score drives ranking.
    #[test]
    fn no_task_tokens_uses_language_score() {
        let rust_profile = make_profile("rust-expert", &["rust"], &["rust", "cargo"]);
        let py_profile = make_profile("python-dev", &["python"], &["python", "django"]);
        let index = PersonaIndex {
            profiles: vec![rust_profile, py_profile],
        };

        let ctx = make_ctx(&[("rust", 1.0)], &[]);
        let weights = PolicyWeights::default();
        let prefs = Preferences::new();
        let ranked = rank(&ctx, &index, &weights, &prefs);

        assert_eq!(ranked[0].persona, "rust-expert");
    }

    /// Preference bias nudges the score.
    #[test]
    fn preference_bias_nudges_score() {
        let a = make_profile("alpha", &["rust"], &["rust"]);
        let b = make_profile("beta", &["rust"], &["rust"]);
        let index = PersonaIndex {
            profiles: vec![a, b],
        };

        let ctx = make_ctx(&[("rust", 1.0)], &[]);
        let weights = PolicyWeights::default();
        let mut prefs = Preferences::new();
        // Strongly bias beta.
        for _ in 0..4 {
            prefs.record_override(Some("alpha"), "beta");
        }

        let ranked = rank(&ctx, &index, &weights, &prefs);
        assert_eq!(ranked[0].persona, "beta");
    }

    /// Empty index returns empty result.
    #[test]
    fn empty_index_returns_empty() {
        let index = PersonaIndex { profiles: vec![] };
        let ctx = make_ctx(&[("rust", 1.0)], &["foo"]);
        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        assert!(ranked.is_empty());
    }

    /// A persona whose primary_intents match the inferred task intent scores higher.
    #[test]
    fn intent_match_boosts_score() {
        use crate::intent::Intent;

        let mut rust = make_profile("rust-expert", &["rust"], &["rust", "cargo"]);
        rust.primary_intents = vec![Intent::Implementation, Intent::Debugging];

        let mut perf = make_profile("perf-expert", &["rust"], &["rust", "profiling"]);
        perf.primary_intents = vec![Intent::Performance];

        let index = PersonaIndex {
            profiles: vec![rust, perf],
        };
        let ctx = ContextSignal {
            project_name: "test".to_string(),
            languages: {
                let mut m = std::collections::BTreeMap::new();
                m.insert("rust".to_string(), 1.0);
                m
            },
            frameworks: vec![],
            task_tokens: vec!["debugging".to_string(), "error".to_string()],
            context_tokens: vec![],
            inferred_intent: Some(Intent::Debugging),
        };

        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        assert_eq!(
            ranked[0].persona, "rust-expert",
            "debugging intent should boost rust-expert"
        );
    }

    /// Anti-keywords penalize the lexical score when task tokens match the blacklist.
    #[test]
    fn anti_keywords_penalize_score() {
        let mut persona_a = make_profile("backend", &["rust"], &["rust", "api"]);
        persona_a.anti_keywords = vec!["css".to_string(), "frontend".to_string()];

        let persona_b = make_profile("frontend", &["typescript"], &["react", "css"]);

        let index = PersonaIndex {
            profiles: vec![persona_a, persona_b],
        };
        let ctx = make_ctx(&[("rust", 1.0)], &["css", "styling", "frontend"]);

        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        let backend_entry = ranked.iter().find(|s| s.persona == "backend").unwrap();
        assert!(
            backend_entry.components.lexical < 0.3,
            "anti-keywords should penalize lexical score"
        );
    }

    /// Anti-keyword matching is case-insensitive: uppercase manifest entries
    /// still penalize lowercase task tokens.
    #[test]
    fn anti_keywords_penalize_case_insensitive() {
        let mut persona_a = make_profile("backend", &["rust"], &["rust", "api"]);
        persona_a.anti_keywords = vec!["CSS".to_string(), "Frontend".to_string()];

        let persona_b = make_profile("frontend", &["typescript"], &["react", "css"]);

        let index = PersonaIndex {
            profiles: vec![persona_a, persona_b],
        };
        let ctx = make_ctx(&[("rust", 1.0)], &["css", "styling", "frontend"]);

        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        let backend_entry = ranked.iter().find(|s| s.persona == "backend").unwrap();
        assert!(
            backend_entry.components.lexical < 0.3,
            "case-insensitive anti-keywords should penalize lexical score"
        );
    }

    /// A dependency token that matches a persona keyword adds a context bonus
    /// and breaks a tie against an otherwise-equal persona.
    #[test]
    fn context_tokens_bonus_rewards_dependency_match() {
        let web = make_profile("web-rust", &["rust"], &["rust", "axum"]);
        let plain = make_profile("plain-rust", &["rust"], &["rust"]);
        let index = PersonaIndex {
            profiles: vec![plain, web],
        };
        let mut ctx = make_ctx(&[("rust", 1.0)], &[]);
        ctx.context_tokens = vec!["axum".to_string(), "tokio".to_string()];

        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        assert_eq!(
            ranked[0].persona, "web-rust",
            "the axum dependency should boost the axum-keyworded persona"
        );
        let web_entry = ranked.iter().find(|s| s.persona == "web-rust").unwrap();
        assert!(web_entry.components.context > 0.0);
    }

    /// An empty context_tokens set contributes no bonus (regression guard).
    #[test]
    fn empty_context_tokens_add_no_bonus() {
        let p = make_profile("rust-expert", &["rust"], &["rust", "axum"]);
        let index = PersonaIndex { profiles: vec![p] };
        let ctx = make_ctx(&[("rust", 1.0)], &[]);
        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        assert_eq!(ranked[0].components.context, 0.0);
    }

    /// A detected build framework (e.g. `cargo`) contributes the same context
    /// bonus as a dependency when it matches a persona keyword.
    #[test]
    fn framework_marker_contributes_context_bonus() {
        let rust_dev = make_profile("rust-dev", &["rust"], &["rust", "cargo"]);
        let plain = make_profile("plain", &["rust"], &["rust"]);
        let index = PersonaIndex {
            profiles: vec![plain, rust_dev],
        };
        let mut ctx = make_ctx(&[("rust", 1.0)], &[]);
        ctx.frameworks = vec!["cargo".to_string()];

        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        let entry = ranked.iter().find(|s| s.persona == "rust-dev").unwrap();
        assert!(
            entry.components.context > 0.0,
            "the cargo build framework should give the cargo-keyworded persona a bonus"
        );
        assert_eq!(ranked[0].persona, "rust-dev");
    }

    /// Without an embedder the semantic channel is inert: `rank` (and any caller
    /// passing `None`) yields a semantic component of exactly 0.0. Regression
    /// guard against the semantic bonus leaking into the default scoring path.
    #[test]
    fn semantic_channel_is_zero_without_embedder() {
        let p = make_profile("rust-dev", &["rust"], &["rust", "cargo", "ownership"]);
        let index = PersonaIndex { profiles: vec![p] };
        let ctx = make_ctx(&[("rust", 1.0)], &["ownership"]);
        let ranked = rank(&ctx, &index, &PolicyWeights::default(), &Preferences::new());
        assert_eq!(ranked[0].components.semantic, 0.0);
    }

    /// With a (mock) embedder, a task hint that overlaps a persona's text earns a
    /// positive semantic bonus that lifts the blended score above the no-embedder
    /// baseline.
    #[test]
    fn semantic_channel_rewards_related_hint() {
        use crate::embed::BagOfWordsEmbedder;

        let p = make_profile("rust-dev", &["rust"], &["rust", "cargo", "ownership"]);
        let index = PersonaIndex { profiles: vec![p] };
        let ctx = make_ctx(&[("rust", 1.0)], &["ownership", "cargo"]);
        let weights = PolicyWeights::default();

        let baseline = rank(&ctx, &index, &weights, &Preferences::new());

        let emb = BagOfWordsEmbedder;
        let ranked = rank_with_embedder(&ctx, &index, &weights, &Preferences::new(), Some(&emb));

        assert!(
            ranked[0].components.semantic > 0.0,
            "an overlapping hint should produce a semantic bonus"
        );
        assert!(
            ranked[0].score >= baseline[0].score,
            "the semantic bonus must not lower the score"
        );
    }
}
