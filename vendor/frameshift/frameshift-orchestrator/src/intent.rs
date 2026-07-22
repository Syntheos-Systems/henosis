//! Task intent classification from token analysis.

use serde::{Deserialize, Serialize};

/// Recognized task intent categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Building new functionality.
    Implementation,
    /// Diagnosing and fixing bugs.
    Debugging,
    /// Inspecting code for correctness or style.
    Review,
    /// Assessing or hardening security posture.
    Security,
    /// Producing documentation, READMEs, changelogs.
    Writing,
    /// Infrastructure, deployment, CI/CD.
    Ops,
    /// Writing or fixing tests.
    Testing,
    /// Restructuring without changing behavior.
    Refactoring,
    /// Profiling, benchmarking, optimization.
    Performance,
    /// Architecture, planning, design.
    Design,
}

/// Static mapping from each Intent variant to the tokens that signal it.
///
/// Each entry is `(Intent, &[&str])` where the slice contains lowercase trigger
/// tokens. Matching is done case-insensitively against lowercased input tokens.
static INTENT_TRIGGERS: &[(Intent, &[&str])] = &[
    (
        Intent::Debugging,
        &[
            "debug",
            "debugging",
            "error",
            "crash",
            "panic",
            "backtrace",
            "stacktrace",
            "fix",
            "bug",
            "trace",
            "segfault",
            "coredump",
            "breakpoint",
            "diagnose",
        ],
    ),
    (
        Intent::Security,
        &[
            "security",
            "vulnerability",
            "cve",
            "audit",
            "pentest",
            "exploit",
            "hardening",
            "threat",
            "compliance",
            "owasp",
            "xss",
            "injection",
            "authentication",
            "authorization",
        ],
    ),
    (
        Intent::Testing,
        &[
            "test",
            "testing",
            "unittest",
            "integration",
            "coverage",
            "assertion",
            "mock",
            "fixture",
            "snapshot",
            "e2e",
            "tdd",
        ],
    ),
    (
        Intent::Writing,
        &[
            "docs",
            "documentation",
            "readme",
            "tutorial",
            "changelog",
            "prose",
            "copywriting",
            "draft",
            "publish",
            "article",
            "essay",
        ],
    ),
    (
        Intent::Performance,
        &[
            "perf",
            "performance",
            "benchmark",
            "profiling",
            "flamegraph",
            "latency",
            "throughput",
            "optimization",
            "hotpath",
            "bottleneck",
        ],
    ),
    (
        Intent::Ops,
        &[
            "deploy",
            "deployment",
            "infrastructure",
            "pipeline",
            "container",
            "docker",
            "kubernetes",
            "systemd",
            "nginx",
            "terraform",
            "ansible",
        ],
    ),
    (
        Intent::Refactoring,
        &[
            "refactor",
            "refactoring",
            "cleanup",
            "restructure",
            "extract",
            "inline",
            "rename",
            "decompose",
            "simplify",
        ],
    ),
    (
        Intent::Review,
        &[
            "review",
            "reviewing",
            "code review",
            "pullrequest",
            "approve",
            "feedback",
            "critique",
        ],
    ),
    (
        Intent::Implementation,
        &[
            "implement",
            "implementing",
            "build",
            "create",
            "add",
            "feature",
            "scaffold",
            "wire",
            "integrate",
        ],
    ),
    (
        Intent::Design,
        &[
            "design",
            "architect",
            "architecture",
            "plan",
            "planning",
            "spec",
            "specification",
            "rfc",
            "proposal",
        ],
    ),
];

/// Classify a slice of task tokens into the most likely Intent.
///
/// Counts trigger hits per intent and returns the intent with the highest count.
/// Returns `None` if the token slice is empty or no trigger matches any intent.
pub fn classify(tokens: &[String]) -> Option<Intent> {
    if tokens.is_empty() {
        return None;
    }

    // Lowercase all tokens once to avoid repeated allocation per comparison.
    let lowered: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();

    let mut best_intent: Option<Intent> = None;
    let mut best_count: usize = 0;

    for (intent, triggers) in INTENT_TRIGGERS {
        let count = lowered
            .iter()
            .filter(|token| triggers.contains(&token.as_str()))
            .count();

        if count > best_count {
            best_count = count;
            best_intent = Some(*intent);
        }
    }

    best_intent
}

/// Map an Intent variant to a stable numeric discriminant for canonical ordering.
///
/// Used by `relatedness` to produce a canonical (min, max) pair without depending
/// on the enum's declaration order being stable in the compiled output.
fn variant_index(intent: Intent) -> u8 {
    match intent {
        Intent::Implementation => 0,
        Intent::Debugging => 1,
        Intent::Review => 2,
        Intent::Security => 3,
        Intent::Writing => 4,
        Intent::Ops => 5,
        Intent::Testing => 6,
        Intent::Refactoring => 7,
        Intent::Performance => 8,
        Intent::Design => 9,
    }
}

/// Return the variant with the smaller discriminant index of the two.
fn min_variant(a: Intent, b: Intent) -> Intent {
    if variant_index(a) <= variant_index(b) {
        a
    } else {
        b
    }
}

/// Return the variant with the larger discriminant index of the two.
fn max_variant(a: Intent, b: Intent) -> Intent {
    if variant_index(a) >= variant_index(b) {
        a
    } else {
        b
    }
}

/// Compute a [0.0, 1.0] relatedness score between two intents.
///
/// Returns 1.0 for identical intents, a fixed adjacency value for known related
/// pairs, and 0.0 for all other combinations. Commutative: order of arguments
/// does not affect the result.
pub fn relatedness(a: Intent, b: Intent) -> f32 {
    if a == b {
        return 1.0;
    }

    // Canonical ordering ensures (min, max) matches the table regardless of
    // which argument order the caller uses.
    let lo = min_variant(a, b);
    let hi = max_variant(a, b);

    match (lo, hi) {
        // Debugging <-> Implementation
        (Intent::Implementation, Intent::Debugging) => 0.5,
        // Debugging <-> Testing
        (Intent::Debugging, Intent::Testing) => 0.5,
        // Review <-> Security
        (Intent::Review, Intent::Security) => 0.5,
        // Implementation <-> Refactoring
        (Intent::Implementation, Intent::Refactoring) => 0.5,
        // Performance <-> Testing
        (Intent::Testing, Intent::Performance) => 0.3,
        // Design <-> Writing
        (Intent::Writing, Intent::Design) => 0.3,
        // All other pairs are unrelated.
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Debugging tokens classify to Debugging intent.
    #[test]
    fn classifies_debugging_intent() {
        let tokens = vec![
            "debugging".to_string(),
            "rust".to_string(),
            "error".to_string(),
        ];
        assert_eq!(classify(&tokens), Some(Intent::Debugging));
    }

    /// Mixed security and review tokens classify to the higher-hit intent (Security).
    #[test]
    fn classifies_security_intent() {
        let tokens = vec![
            "reviewing".to_string(),
            "security".to_string(),
            "vulnerability".to_string(),
        ];
        assert_eq!(classify(&tokens), Some(Intent::Security));
    }

    /// Writing-adjacent tokens classify to Writing.
    #[test]
    fn classifies_writing_intent() {
        let tokens = vec!["writing".to_string(), "documentation".to_string()];
        assert_eq!(classify(&tokens), Some(Intent::Writing));
    }

    /// Empty token slice always returns None.
    #[test]
    fn returns_none_for_empty_tokens() {
        let tokens: Vec<String> = vec![];
        assert_eq!(classify(&tokens), None);
    }

    /// Generic tokens that hit no trigger list return None.
    #[test]
    fn returns_none_for_ambiguous_tokens() {
        let tokens = vec!["the".to_string(), "code".to_string()];
        assert_eq!(classify(&tokens), None);
    }

    /// Adjacent intent pairs score above zero, and scoring is commutative.
    #[test]
    fn related_intents_score_above_zero() {
        assert_eq!(relatedness(Intent::Debugging, Intent::Implementation), 0.5);
        assert_eq!(relatedness(Intent::Implementation, Intent::Debugging), 0.5);
        assert_eq!(relatedness(Intent::Review, Intent::Security), 0.5);
    }

    /// Unrelated pairs score exactly zero.
    #[test]
    fn unrelated_intents_score_zero() {
        assert_eq!(relatedness(Intent::Debugging, Intent::Writing), 0.0);
        assert_eq!(relatedness(Intent::Ops, Intent::Design), 0.0);
    }

    /// Identical intents always score 1.0.
    #[test]
    fn same_intent_scores_one() {
        assert_eq!(relatedness(Intent::Debugging, Intent::Debugging), 1.0);
    }
}
