//! Keyword relevance scoring between a message and a persona's interests.
//!
//! v1 is lexical (no embeddings): it measures overlap between the message's
//! terms and the persona's interest terms. The `score` signature is the stable
//! seam -- an embedding-based implementation can replace the body later without
//! changing callers.

/// English stopwords excluded from both message and interest tokenization.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "can",
    "had", "her", "was", "one", "our", "out", "day", "get", "has",
    "him", "his", "how", "its", "may", "now", "old", "see", "two",
    "who", "did", "she", "use", "way", "will", "with", "this", "that",
    "from", "have", "been", "they", "were", "said", "each", "which",
    "their", "when", "what", "your", "make", "like", "into", "time",
    "look", "more", "write", "than", "been", "call", "first", "long",
    "down", "side", "been", "now", "come", "made", "over", "such",
    "also", "here", "just", "know", "take", "some", "only", "both",
    "then", "very", "even", "much", "back", "well", "must", "about",
    "good", "after", "those", "tell", "does", "gave", "give",
];

/// Tokenize `text`: lowercase, split on non-alphanumeric chars, drop tokens
/// shorter than 3 characters, and drop English stopwords.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|tok| {
            if tok.len() < 3 {
                return None;
            }
            let lower = tok.to_lowercase();
            if STOPWORDS.contains(&lower.as_str()) {
                None
            } else {
                Some(lower)
            }
        })
        .collect()
}

/// Return `true` if `msg_token` matches `interest_token` by exact equality or
/// one being a substring of the other (stem-ish overlap).
fn tokens_match(msg_token: &str, interest_token: &str) -> bool {
    msg_token == interest_token
        || msg_token.contains(interest_token)
        || interest_token.contains(msg_token)
}

/// Keyword relevance of `message` to `persona_interests`, in `[0.0, 1.0]`.
///
/// - Returns `0.5` when `persona_interests` is empty (no signal; should not
///   suppress an agent).
/// - Returns `0.0` when the message contains no meaningful tokens after
///   stopword removal.
/// - Otherwise returns the fraction of meaningful message tokens that match at
///   least one interest term, scaled toward 1.0 as more distinct interests are
///   matched.
///
/// Higher means the message is more on-topic for an agent holding that persona.
pub fn score(message: &str, persona_interests: &[String]) -> f64 {
    // No interests => no signal; treat as neutral.
    if persona_interests.is_empty() {
        return 0.5;
    }

    let msg_tokens = tokenize(message);

    // No meaningful words in message => definitively off-topic.
    if msg_tokens.is_empty() {
        return 0.0;
    }

    // Build flat token list for all interest terms.
    let interest_tokens: Vec<String> = persona_interests
        .iter()
        .flat_map(|interest| tokenize(interest))
        .collect();

    if interest_tokens.is_empty() {
        // Interests exist but collapse entirely to stopwords/short tokens.
        return 0.5;
    }

    // Count message tokens that hit at least one interest token.
    let matching_msg: usize = msg_tokens
        .iter()
        .filter(|mt| interest_tokens.iter().any(|it| tokens_match(mt, it)))
        .count();

    // Count how many distinct interests (by index) were touched -- matching
    // more separate interests is stronger signal.
    let interests_hit: usize = persona_interests
        .iter()
        .filter(|interest| {
            let itoks = tokenize(interest);
            itoks
                .iter()
                .any(|it| msg_tokens.iter().any(|mt| tokens_match(mt, it)))
        })
        .count();

    // Base score: fraction of message tokens covered by interests.
    let token_coverage = matching_msg as f64 / msg_tokens.len() as f64;

    // Breadth bonus: fraction of distinct interests touched (biases upward when
    // the message is broadly on-topic rather than hitting one interest term many
    // times).
    let interest_breadth = interests_hit as f64 / persona_interests.len() as f64;

    // Weighted blend: 70% token coverage, 30% interest breadth.
    let raw = 0.7 * token_coverage + 0.3 * interest_breadth;

    // Clamp to [0.0, 1.0] for safety.
    raw.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a `Vec<String>` from string literals.
    fn interests(terms: &[&str]) -> Vec<String> {
        terms.iter().map(|s| s.to_string()).collect()
    }

    /// On-topic message scores higher than an off-topic one for the same interests.
    #[test]
    fn on_topic_beats_off_topic() {
        let interests_list = interests(&["machine learning", "neural networks", "transformers"]);

        let on_topic = score(
            "The transformer architecture changed how neural networks are trained",
            &interests_list,
        );
        let off_topic = score(
            "I enjoy hiking and camping in the mountains on weekends",
            &interests_list,
        );

        assert!(
            on_topic > off_topic,
            "on_topic={on_topic} should exceed off_topic={off_topic}"
        );
    }

    /// Empty interests list always returns the neutral 0.5 value.
    #[test]
    fn empty_interests_returns_neutral() {
        assert_eq!(score("hello world this is a message", &[]), 0.5);
        assert_eq!(score("", &[]), 0.5);
    }

    /// A message with no meaningful tokens (empty or punctuation-only) returns 0.0.
    #[test]
    fn empty_message_returns_zero() {
        let ints = interests(&["rust", "programming"]);
        assert_eq!(score("", &ints), 0.0);
        assert_eq!(score("... !!! ???", &ints), 0.0);
        assert_eq!(score("-- -- --", &ints), 0.0);
    }

    /// Result is always within [0.0, 1.0] for arbitrary inputs.
    #[test]
    fn result_bounded() {
        let cases: &[(&str, &[&str])] = &[
            ("hello world", &["hello", "world"]),
            ("completely unrelated text here", &["rust", "async", "tokio"]),
            ("rust async tokio runtime executor spawn", &["rust", "async", "tokio"]),
            ("", &["anything"]),
            ("something", &[]),
        ];

        for (msg, ints) in cases {
            let s = score(msg, &interests(ints));
            assert!(
                (0.0..=1.0).contains(&s),
                "score({msg:?}, {ints:?}) = {s} is out of [0,1]"
            );
        }
    }

    /// A message that exactly repeats an interest term scores above 0.
    #[test]
    fn direct_interest_match_scores_nonzero() {
        let ints = interests(&["cryptography"]);
        let s = score("cryptography is important for security", &ints);
        assert!(s > 0.0, "expected positive score, got {s}");
    }

    /// Substring matching works: "program" matches interest "programming".
    #[test]
    fn substring_match_works() {
        let ints = interests(&["programming"]);
        let s = score("she loves program design and architecture", &ints);
        assert!(s > 0.0, "substring match expected positive score, got {s}");
    }
}
