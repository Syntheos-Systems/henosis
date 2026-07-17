//! Cross-agent echo suppression (2026-07-17 design spec, P3 / F4).
//!
//! Nothing in botcore compared an agent's candidate output against what its
//! *peers* just said, so Zim reproduced GIR's line near-verbatim unnoticed.
//! For working agents the failure mode is two agents independently producing
//! the same proposal and both believing they contributed.
//!
//! This module is the cheap first pass the spec calls for: normalized token
//! overlap (Jaccard) between a candidate response and recent peer messages.
//! Embedding-based similarity (memory 27272) remains future work and is
//! recorded as such in the design doc addendum (spec P4).

use std::collections::HashSet;

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
}
