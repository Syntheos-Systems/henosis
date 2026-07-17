//! Cross-agent echo suppression (2026-07-17 design spec, P3 / F4).
//!
//! Nothing in botcore compared an agent's candidate output against what its
//! *peers* just said, so Zim reproduced GIR's line near-verbatim unnoticed.
//! For working agents the failure mode is two agents independently producing
//! the same proposal and both believing they contributed.
//!
//! Two detection tiers live here:
//!
//! - The token-overlap (Jaccard) pass the spec called the cheap first cut.
//! - [`EchoDetector`]: the embedding-based tier (memory 27272, spec P4)
//!   filling the `echo::similarity` seam the design doc addendum named.
//!   When an embedder is configured, candidates are compared to recent peer
//!   posts by cosine similarity, which catches paraphrased echoes token
//!   overlap cannot. Without one -- or when an embed call fails -- detection
//!   degrades to the Jaccard pass, never to a false suppression.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::embedding::{cosine, Embedder};

/// Minimum candidate token count before suppression may trigger. Short
/// acknowledgments ("yes exactly", "good point") legitimately share most of
/// their few tokens with longer peer messages and must not be nuked.
const MIN_SUPPRESSIBLE_TOKENS: usize = 6;

/// Lowercased alphanumeric tokens of length >= 3. Short function words
/// ("a", "of", "is") carry no echo signal and only dilute the measure.
fn tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_owned)
        .collect()
}

/// Jaccard similarity of the two texts' token sets: |A n B| / |A u B|.
/// Returns 0.0 when either side has no tokens.
pub fn similarity(a: &str, b: &str) -> f64 {
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.intersection(&tb).count();
    let union = ta.len() + tb.len() - intersection;
    intersection as f64 / union as f64
}

/// True when the candidate substantially reproduces any recent peer message
/// (max pairwise similarity >= threshold). Callers must exclude the
/// candidate author's own messages from `recent_peer_texts` (self-repetition
/// is a different concern) and should exempt consensus votes before calling:
/// [AGREE] messages are legitimately similar to each other.
pub fn is_echo(candidate: &str, recent_peer_texts: &[&str], threshold: f64) -> bool {
    if tokens(candidate).len() < MIN_SUPPRESSIBLE_TOKENS {
        return false;
    }
    recent_peer_texts
        .iter()
        .any(|peer| similarity(candidate, peer) >= threshold)
}

/// Stateful echo detector combining the semantic (embedding) tier with the
/// token-overlap fallback. Owned by the room; one instance per channel.
///
/// The embedding cache is keyed by exact message text and pruned to the
/// current peer window after every check, so it never outgrows the recent-post
/// ring buffer that feeds it.
pub struct EchoDetector {
    /// Optional semantic tier. `None` means token-overlap only.
    embedder: Option<Arc<dyn Embedder>>,
    /// Cosine similarity at or above which a candidate is an echo
    /// (parent design: 0.85).
    semantic_threshold: f64,
    /// Jaccard similarity threshold for the fallback tier.
    token_threshold: f64,
    /// Embeddings of recently seen texts, keyed by exact text.
    cache: HashMap<String, Vec<f32>>,
}

/// Detection entry point and the semantic-tier internals.
impl EchoDetector {
    /// Build a detector. `embedder` = `None` preserves the pure
    /// token-overlap behavior byte-for-byte.
    pub fn new(
        embedder: Option<Arc<dyn Embedder>>,
        semantic_threshold: f64,
        token_threshold: f64,
    ) -> Self {
        Self {
            embedder,
            semantic_threshold,
            token_threshold,
            cache: HashMap::new(),
        }
    }

    /// True when the candidate substantially reproduces any recent peer
    /// message. Semantic tier when configured; token overlap otherwise. An
    /// embed failure degrades to token overlap for this check and logs a
    /// warning -- an unreachable embeddings endpoint must never suppress a
    /// post by itself, and must never let a verbatim echo through either.
    pub async fn is_echo(&mut self, candidate: &str, recent_peer_texts: &[&str]) -> bool {
        if tokens(candidate).len() < MIN_SUPPRESSIBLE_TOKENS {
            return false;
        }
        if recent_peer_texts.is_empty() {
            return false;
        }
        if self.embedder.is_some() {
            match self.max_semantic(candidate, recent_peer_texts).await {
                Ok(max) => return max >= self.semantic_threshold,
                Err(e) => {
                    tracing::warn!("semantic echo check failed, falling back to token overlap: {e}");
                }
            }
        }
        is_echo(candidate, recent_peer_texts, self.token_threshold)
    }

    /// Maximum non-negative cosine similarity between the candidate and the
    /// peer window (floored at 0.0 deliberately: negative similarity means
    /// "opposite", which must never edge toward suppression). Fills the
    /// cache for peers on demand. The candidate itself is not cached:
    /// suppressed candidates never become peers, and posted ones re-enter
    /// via the peer side on the next check.
    async fn max_semantic(
        &mut self,
        candidate: &str,
        recent_peer_texts: &[&str],
    ) -> Result<f64, crate::error::BridgeError> {
        let embedder = self
            .embedder
            .as_ref()
            .expect("max_semantic called without embedder")
            .clone();

        // Prune BEFORE embedding, not after: pruning on the exit path is
        // skipped whenever an embed call errors out mid-loop, letting stale
        // entries outlive the ring-buffer bound (adversarial review
        // finding). Entry-side pruning holds the invariant on every path.
        let live: HashSet<&str> = recent_peer_texts.iter().copied().collect();
        self.cache.retain(|text, _| live.contains(text.as_str()));

        let candidate_vec = match self.cache.get(candidate) {
            Some(v) => v.clone(),
            None => embedder.embed(candidate).await?,
        };

        let mut max = 0.0f64;
        for peer in recent_peer_texts {
            if !self.cache.contains_key(*peer) {
                let vec = embedder.embed(peer).await?;
                self.cache.insert((*peer).to_string(), vec);
            }
            let peer_vec = &self.cache[*peer];
            if peer_vec.len() != candidate_vec.len() {
                tracing::warn!(
                    "embedding dimension mismatch ({} vs {}), scoring 0",
                    peer_vec.len(),
                    candidate_vec.len()
                );
            }
            let score = cosine(&candidate_vec, peer_vec);
            if score > max {
                max = score;
            }
        }
        Ok(max)
    }
}

/// Unit tests using the production echo strings from the 2026-07-16 botcore
/// incident plus boundary cases.
#[cfg(test)]
mod tests {
    use super::{is_echo, similarity};

    /// The line GIR posted at 12:54:04 in the measured incident.
    const GIR_LINE: &str = "Selection pressure weeded out the weak bots and kept the best ones running strong";
    /// The near-verbatim reproduction Zim posted 35 seconds later.
    const ZIM_LINE: &str = "Selection pressure weeded out the weak bots and kept the best";

    /// Verifies the measured production echo pair scores well above the
    /// default threshold and gets suppressed.
    #[test]
    fn test_production_echo_pair_is_suppressed() {
        assert!(similarity(GIR_LINE, ZIM_LINE) >= 0.7);
        assert!(is_echo(ZIM_LINE, &[GIR_LINE], 0.5));
    }

    /// Verifies a genuine engagement with the same topic (Eidolon's reply,
    /// which named peers instead of echoing) is not suppressed.
    #[test]
    fn test_engaged_reply_on_same_topic_passes() {
        let eidolon = "Selection pressure kept you, GIR, and Sam because you each answer differently";
        assert!(!is_echo(eidolon, &[GIR_LINE], 0.5));
    }

    /// Verifies unrelated content scores near zero.
    #[test]
    fn test_unrelated_content_scores_near_zero() {
        assert!(similarity(GIR_LINE, "The deploy pipeline needs a rollback path before Friday") < 0.1);
    }

    /// Verifies short acknowledgments are never suppressed even when their
    /// few tokens all appear in a longer peer message.
    #[test]
    fn test_short_acknowledgment_is_never_suppressed() {
        assert!(!is_echo("the best ones", &[GIR_LINE], 0.3));
    }

    /// Verifies empty and token-free inputs are handled without panicking.
    #[test]
    fn test_degenerate_inputs() {
        assert_eq!(similarity("", ""), 0.0);
        assert_eq!(similarity("!!", "??"), 0.0);
        assert!(!is_echo("", &[GIR_LINE], 0.5));
        assert!(!is_echo(GIR_LINE, &[], 0.5));
    }

    /// Verifies case and punctuation differences do not defeat detection.
    #[test]
    fn test_normalization_is_case_and_punctuation_insensitive() {
        let shouty = "SELECTION PRESSURE weeded-out the WEAK bots, and kept the BEST!!";
        assert!(similarity(GIR_LINE, shouty) >= 0.7);
    }

    use super::EchoDetector;
    use crate::embedding::Embedder;
    use crate::error::BridgeError;
    use async_trait::async_trait;
    use std::sync::Arc;

    /// Embedder stub mapping known texts to fixed vectors, so semantic
    /// scores are deterministic without a network.
    struct StubEmbedder;

    /// Maps paraphrase-pair texts to nearby vectors and everything else far away.
    #[async_trait]
    impl Embedder for StubEmbedder {
        /// Returns canned vectors: the two paraphrase texts point the same
        /// way, unrelated text is orthogonal.
        async fn embed(&self, text: &str) -> Result<Vec<f32>, BridgeError> {
            if text.contains("weeded") || text.contains("filtered out the weak") {
                Ok(vec![1.0, 0.05])
            } else {
                Ok(vec![0.0, 1.0])
            }
        }
    }

    /// Embedder stub that always fails, for fallback-path testing.
    struct FailingEmbedder;

    /// Fails every call with an embedding error.
    #[async_trait]
    impl Embedder for FailingEmbedder {
        /// Always errors.
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, BridgeError> {
            Err(BridgeError::Embedding("endpoint down".into()))
        }
    }

    /// Verifies the semantic tier catches a paraphrased echo that token
    /// overlap misses -- the exact gap embeddings were specified to close.
    #[tokio::test]
    async fn test_semantic_tier_catches_paraphrase_jaccard_misses() {
        let paraphrase = "Natural selection filtered out the weak machines while retaining top performers";
        // Token overlap alone does NOT flag this pair at the default threshold.
        assert!(!is_echo(paraphrase, &[GIR_LINE], 0.5));

        let mut det = EchoDetector::new(Some(Arc::new(StubEmbedder)), 0.85, 0.5);
        assert!(det.is_echo(paraphrase, &[GIR_LINE]).await);
    }

    /// Verifies unrelated content passes the semantic tier.
    #[tokio::test]
    async fn test_semantic_tier_passes_unrelated_content() {
        let mut det = EchoDetector::new(Some(Arc::new(StubEmbedder)), 0.85, 0.5);
        assert!(
            !det.is_echo("The deploy pipeline needs a rollback path before Friday", &[GIR_LINE])
                .await
        );
    }

    /// Verifies an embed failure degrades to the token-overlap tier instead
    /// of suppressing or panicking: the verbatim production echo is still
    /// caught, unrelated content still passes.
    #[tokio::test]
    async fn test_embed_failure_falls_back_to_token_overlap() {
        let mut det = EchoDetector::new(Some(Arc::new(FailingEmbedder)), 0.85, 0.5);
        assert!(det.is_echo(ZIM_LINE, &[GIR_LINE]).await);
        assert!(
            !det.is_echo("The deploy pipeline needs a rollback path before Friday", &[GIR_LINE])
                .await
        );
    }

    /// Verifies no embedder preserves the pure token-overlap behavior.
    #[tokio::test]
    async fn test_no_embedder_is_token_overlap_only() {
        let mut det = EchoDetector::new(None, 0.85, 0.5);
        assert!(det.is_echo(ZIM_LINE, &[GIR_LINE]).await);
        let paraphrase = "Natural selection filtered out the weak machines while retaining top performers";
        assert!(!det.is_echo(paraphrase, &[GIR_LINE]).await);
    }

    /// Verifies the short-message guard applies before the semantic tier.
    #[tokio::test]
    async fn test_short_candidate_skips_semantic_tier() {
        let mut det = EchoDetector::new(Some(Arc::new(FailingEmbedder)), 0.85, 0.5);
        // Five tokens or fewer: never suppressed, embedder never consulted
        // (FailingEmbedder would otherwise log a fallback).
        assert!(!det.is_echo("the best ones", &[GIR_LINE]).await);
    }
}
