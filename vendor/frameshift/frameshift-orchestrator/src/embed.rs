//! Semantic embedding abstraction for meaning-based persona matching.
//!
//! This module stays dependency-free: it defines the [`Embedder`] trait, the
//! cosine-similarity math the policy scorer uses, and the persistence-backed
//! [`CachedEmbedder`] memoization wrapper. The concrete engine lives in the
//! separate `frameshift-embed-candle` crate (a MiniLM sentence transformer on
//! pure-Rust candle) and is compiled only behind consumers' optional
//! `embeddings` cargo feature, so the default workspace build carries no ML
//! stack. Without an embedder the semantic channel contributes `0.0`
//! everywhere and selection behavior is unchanged.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::{fs, io};

/// Produces a dense vector embedding for a piece of text.
///
/// Implementations map natural-language text to a fixed-dimensional vector
/// whose geometry encodes meaning, so that related texts have a high cosine
/// similarity. The trait is object-safe (`&dyn Embedder`) so the scorer can
/// accept an optional embedder without being generic over its type. Two calls
/// with equal input should return equal output (determinism), and all vectors
/// from a given embedder should share one dimensionality.
pub trait Embedder {
    /// Return the embedding vector for `text`.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// A reference to an embedder is itself an embedder, so wrappers like
/// [`CachedEmbedder`] can either own or borrow their inner model.
impl<T: Embedder + ?Sized> Embedder for &T {
    /// Delegate to the referenced embedder.
    fn embed(&self, text: &str) -> Vec<f32> {
        (**self).embed(text)
    }
}

/// Cosine similarity between two equal-length vectors, clamped to `[0.0, 1.0]`.
///
/// Returns `0.0` (a safe no-signal value) rather than panicking or producing
/// `NaN` when the inputs are degenerate: empty vectors, vectors of differing
/// length, or a vector whose L2 norm is effectively zero. Negative cosine
/// (anti-correlated vectors) is clamped to `0.0` because the scorer treats this
/// as an additive similarity bonus, never a penalty.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(0.0, 1.0)
}

/// Embed both texts with `embedder` and return their cosine similarity in
/// `[0.0, 1.0]`.
///
/// Convenience wrapper over [`cosine_similarity`]. Inherits its degenerate-case
/// handling, so an embedder that returns empty or mismatched-length vectors
/// yields `0.0` rather than an error.
pub fn semantic_similarity(embedder: &dyn Embedder, a: &str, b: &str) -> f32 {
    let va = embedder.embed(a);
    let vb = embedder.embed(b);
    cosine_similarity(&va, &vb)
}

/// Longest text (in bytes) admitted to the cache.
///
/// Oversized texts are still embedded, just never cached: caller-supplied task
/// strings (CLI `--task`, the MCP `task` argument) are unbounded, and admitting
/// them would let a single chatty client balloon the cache file that every
/// later construction has to parse. Persona corpus texts and reasonable task
/// hints fit comfortably under this cap.
pub const MAX_CACHED_TEXT_LEN: usize = 1024;

/// Hard ceiling on the number of cached entries.
///
/// Once reached, new texts still embed correctly but are no longer admitted
/// (no eviction: the hot set -- the persona corpus -- is embedded first in
/// every selection pass, so freeze-when-full preserves exactly the entries
/// worth keeping). Bounds both the on-disk file and the per-construction
/// parse cost.
pub const MAX_CACHE_ENTRIES: usize = 1024;

/// A memoizing wrapper around any [`Embedder`], persisted to a JSON file.
///
/// Real embedding models are expensive to invoke: without a cache, every
/// `select` re-embeds the full persona corpus (measured at ~17 s warm for 37
/// personas with the candle MiniLM backend). Persona texts and repeated task
/// phrasings are stable, so this wrapper keys vectors by the exact input text,
/// serving repeats from memory and, across process lifetimes, from a cache
/// file -- the inner model runs once per distinct text.
///
/// The cache file is best-effort: a missing or corrupt file degrades to an
/// empty cache, and writes go through a per-process temp file + atomic rename,
/// so concurrent writers race harmlessly (last write wins, entries are
/// re-computable) and a reader never observes a torn file. Growth is bounded
/// by [`MAX_CACHED_TEXT_LEN`] and [`MAX_CACHE_ENTRIES`]. The file must be
/// scoped to one model -- the format does not self-describe its model, so
/// pointing two different models at one path would serve vectors from the
/// wrong geometry; `frameshift_embed_candle::default_cache_path` derives the
/// filename from the model id for exactly this reason.
pub struct CachedEmbedder<E> {
    /// The wrapped embedder that produces vectors on a cache miss. May be an
    /// owned model or a borrow (see the blanket `impl Embedder for &T`).
    inner: E,
    /// Location of the persisted text -> vector map (JSON).
    cache_path: PathBuf,
    /// In-memory view of the cache, pre-loaded from `cache_path` at
    /// construction and appended to on every admitted miss.
    entries: Mutex<HashMap<String, Vec<f32>>>,
}

/// Construction and persistence for the cache wrapper.
impl<E: Embedder> CachedEmbedder<E> {
    /// Wrap `inner`, loading any previously persisted entries from
    /// `cache_path`. A missing or unparsable file yields an empty cache.
    pub fn new(inner: E, cache_path: PathBuf) -> Self {
        let entries = fs::read_to_string(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self {
            inner,
            cache_path,
            entries: Mutex::new(entries),
        }
    }

    /// Lock the entry map, recovering from poisoning.
    ///
    /// A poisoned lock only means another thread panicked mid-operation; the
    /// map has no invariant a partial insert could violate, so adopting its
    /// state is strictly better than panicking on every later embed.
    fn lock_entries(&self) -> MutexGuard<'_, HashMap<String, Vec<f32>>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Persist pre-serialized cache content via per-process temp file + rename.
    ///
    /// The temp name embeds the PID so concurrent processes never truncate
    /// each other's in-flight write; rename is atomic, so readers only ever
    /// see a complete file. On Unix the cache directory is created `0o700`
    /// and the file `0o600` -- task descriptions can be sensitive, matching
    /// the daemon's socket-dir hardening. Best-effort by design: an
    /// unwritable cache location must never fail an embed, so errors are
    /// swallowed after a `tracing` warning.
    fn persist_raw(&self, raw: &str) {
        if let Some(parent) = self.cache_path.parent() {
            if create_private_dir_all(parent).is_err() {
                return;
            }
        }
        let tmp = self
            .cache_path
            .with_extension(format!("tmp.{}", std::process::id()));
        if write_private_file(&tmp, raw.as_bytes()).is_err() {
            tracing::warn!(path = %self.cache_path.display(), "embedding cache not writable");
            return;
        }
        if fs::rename(&tmp, &self.cache_path).is_err() {
            tracing::warn!(path = %self.cache_path.display(), "embedding cache rename failed");
        }
    }
}

/// The cache wrapper is itself an embedder, so it drops in anywhere the
/// wrapped model would be used.
impl<E: Embedder> Embedder for CachedEmbedder<E> {
    /// Return the cached vector for `text`, running the inner embedder on a
    /// miss and persisting the result only when the text is admissible
    /// (within [`MAX_CACHED_TEXT_LEN`], map below [`MAX_CACHE_ENTRIES`]).
    fn embed(&self, text: &str) -> Vec<f32> {
        if let Some(hit) = self.lock_entries().get(text) {
            return hit.clone();
        }

        // Miss: run the model OUTSIDE the lock (inference is the slow part).
        // A concurrent miss of the same text does redundant work but
        // converges to the same value.
        let vector = self.inner.embed(text);

        if text.len() > MAX_CACHED_TEXT_LEN {
            return vector;
        }

        // Insert and serialize under the lock, but perform the file IO
        // outside it so a concurrent cache hit never blocks on disk.
        let serialized = {
            let mut entries = self.lock_entries();
            if entries.len() >= MAX_CACHE_ENTRIES && !entries.contains_key(text) {
                None
            } else {
                entries.insert(text.to_string(), vector.clone());
                serde_json::to_string(&*entries).ok()
            }
        };
        if let Some(raw) = serialized {
            self.persist_raw(&raw);
        }
        vector
    }
}

/// Create `dir` (and parents) restricted to the owner on Unix (`0o700`).
///
/// Pre-existing directories are left untouched -- only directories this call
/// creates get the restrictive mode, so pointing the cache at a shared parent
/// never tightens permissions on a directory the user already owns elsewhere.
fn create_private_dir_all(dir: &std::path::Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Write `contents` to `path`, creating the file owner-readable only on Unix
/// (`0o600`). The mode is applied at open time, so the content is never
/// world-readable even for an instant; rename preserves it.
fn write_private_file(path: &std::path::Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents)
}

/// A deterministic bag-of-words embedder used only by tests.
///
/// Hashes each whitespace-separated, lowercased word into a fixed-width bucket
/// and counts occurrences. It is order-insensitive and dependency-free, so two
/// texts that share words have overlapping nonzero buckets and therefore a
/// positive cosine similarity -- enough to exercise the semantic channel
/// without a real embedding model.
#[cfg(test)]
pub(crate) struct BagOfWordsEmbedder;

#[cfg(test)]
/// Produces deterministic test embeddings with a bag-of-words model.
impl Embedder for BagOfWordsEmbedder {
    /// Embed `text` as a 64-dimensional word-occurrence histogram.
    fn embed(&self, text: &str) -> Vec<f32> {
        const DIM: usize = 64;
        let mut v = vec![0.0f32; DIM];
        for word in text.split_whitespace() {
            let lowered = word.to_lowercase();
            // FNV-1a hash -> bucket index. Stable across runs (no randomness).
            let mut h: u64 = 0xcbf29ce484222325;
            for byte in lowered.bytes() {
                h ^= byte as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            v[(h as usize) % DIM] += 1.0;
        }
        v
    }
}

#[cfg(test)]
/// Verifies embedding similarity and cache behavior.
mod tests {
    use super::*;

    /// Identical vectors have cosine similarity 1.0.
    #[test]
    fn identical_vectors_are_maximally_similar() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    /// Orthogonal vectors have cosine similarity 0.0.
    #[test]
    fn orthogonal_vectors_are_dissimilar() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    /// Anti-correlated vectors clamp to 0.0 (no negative bonus).
    #[test]
    fn anti_correlated_clamps_to_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
    }

    /// Empty, mismatched-length, and zero-norm inputs yield 0.0 (no panic, no NaN).
    #[test]
    fn degenerate_inputs_yield_zero() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    /// A wrapper counting how many times the inner embedder actually runs,
    /// so cache hits are observable.
    struct CountingEmbedder {
        /// The real embedder producing vectors on a miss.
        inner: BagOfWordsEmbedder,
        /// Number of `embed` calls that reached the inner embedder.
        calls: std::sync::atomic::AtomicUsize,
    }

    /// Constructs and inspects the embedder used to count cache misses.
    impl CountingEmbedder {
        /// Fresh counter around the bag-of-words mock.
        fn new() -> Self {
            Self {
                inner: BagOfWordsEmbedder,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        /// How many embeds reached the inner model so far.
        fn count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Counts each delegated embedding operation.
    impl Embedder for CountingEmbedder {
        /// Delegate to the mock, counting the call.
        fn embed(&self, text: &str) -> Vec<f32> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.embed(text)
        }
    }

    /// Repeated embeds of the same text hit the in-memory cache, and a fresh
    /// wrapper over the same cache file hits the on-disk cache -- the inner
    /// model runs exactly once per distinct text across process lifetimes.
    #[test]
    fn cached_embedder_memoizes_in_memory_and_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("embed-cache.json");

        let counting = CountingEmbedder::new();
        let expected = counting.inner.embed("rust cargo clippy");
        {
            let cached = CachedEmbedder::new(&counting, cache_path.clone());
            let first = cached.embed("rust cargo clippy");
            let second = cached.embed("rust cargo clippy");
            assert_eq!(first, expected, "cache must not alter vectors");
            assert_eq!(second, expected);
            assert_eq!(counting.count(), 1, "second embed must be a memory hit");
        }

        // A brand-new wrapper (fresh process, same cache file) must load the
        // persisted entry instead of re-running the model.
        let cached = CachedEmbedder::new(&counting, cache_path);
        let third = cached.embed("rust cargo clippy");
        assert_eq!(third, expected, "disk-loaded vector must round-trip");
        assert_eq!(counting.count(), 1, "disk hit must not reach the model");
    }

    /// Distinct texts each miss once and are cached independently.
    #[test]
    fn cached_embedder_keys_by_exact_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counting = CountingEmbedder::new();
        let cached = CachedEmbedder::new(&counting, dir.path().join("c.json"));

        cached.embed("alpha");
        cached.embed("beta");
        cached.embed("alpha");
        assert_eq!(counting.count(), 2, "two distinct texts -> two model runs");
    }

    /// Oversized texts are embedded correctly but never cached: the inner
    /// model runs every time and nothing is persisted for them.
    #[test]
    fn cached_embedder_skips_caching_oversized_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("c.json");
        let counting = CountingEmbedder::new();
        let cached = CachedEmbedder::new(&counting, cache_path.clone());

        let big = "x ".repeat(MAX_CACHED_TEXT_LEN); // 2x the cap in bytes
        let expected = counting.inner.embed(&big);
        assert_eq!(
            cached.embed(&big),
            expected,
            "oversized embed must be correct"
        );
        assert_eq!(cached.embed(&big), expected);
        assert_eq!(
            counting.count(),
            2,
            "oversized text must never be served from cache"
        );
        assert!(
            !cache_path.exists(),
            "oversized text must not create a cache file"
        );
    }

    /// The entry count is hard-capped: past the cap, new texts still embed
    /// correctly but are not admitted to the cache, while already-cached
    /// entries keep serving hits.
    #[test]
    fn cached_embedder_caps_total_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counting = CountingEmbedder::new();
        let cached = CachedEmbedder::new(&counting, dir.path().join("c.json"));

        for i in 0..MAX_CACHE_ENTRIES + 10 {
            cached.embed(&format!("text-{i}"));
        }
        let baseline = counting.count();
        assert_eq!(
            baseline,
            MAX_CACHE_ENTRIES + 10,
            "every distinct text misses once"
        );

        // An early (admitted) entry still hits; a post-cap entry misses again.
        cached.embed("text-0");
        assert_eq!(counting.count(), baseline, "admitted entry must still hit");
        cached.embed(&format!("text-{}", MAX_CACHE_ENTRIES + 5));
        assert_eq!(
            counting.count(),
            baseline + 1,
            "post-cap entry is never admitted"
        );
    }

    /// A cache path whose parent directories do not exist yet is created on
    /// the first persist, and a fresh instance disk-hits through it.
    #[test]
    fn cached_embedder_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("nested/deeper/c.json");

        let counting = CountingEmbedder::new();
        CachedEmbedder::new(&counting, cache_path.clone()).embed("alpha");
        assert!(cache_path.is_file(), "persist must create missing parents");

        CachedEmbedder::new(&counting, cache_path).embed("alpha");
        assert_eq!(counting.count(), 1, "fresh instance must disk-hit");
    }

    /// A corrupt cache file degrades to an empty cache: embeds still work and
    /// the file is rewritten with valid content on the next miss.
    #[test]
    fn cached_embedder_survives_corrupt_cache_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_path = dir.path().join("embed-cache.json");
        std::fs::write(&cache_path, b"not json {{{").expect("write garbage");

        let counting = CountingEmbedder::new();
        let cached = CachedEmbedder::new(&counting, cache_path.clone());
        let v = cached.embed("rust cargo clippy");
        assert_eq!(v, counting.inner.embed("rust cargo clippy"));
        assert_eq!(counting.count(), 1);

        // The rewritten file must now be a loadable cache.
        let reloaded = CachedEmbedder::new(&counting, cache_path);
        reloaded.embed("rust cargo clippy");
        assert_eq!(counting.count(), 1, "rewritten cache must serve the hit");
    }

    /// The mock embedder gives a high similarity to identical text and a
    /// positive-but-lower similarity to text that merely shares words.
    #[test]
    fn mock_embedder_reflects_word_overlap() {
        let emb = BagOfWordsEmbedder;
        let same = semantic_similarity(&emb, "rust cargo clippy", "rust cargo clippy");
        let related = semantic_similarity(&emb, "rust cargo clippy", "rust cargo ownership");
        let unrelated = semantic_similarity(&emb, "rust cargo clippy", "tacos");

        assert!((same - 1.0).abs() < 1e-6, "identical text -> 1.0");
        assert!(related > 0.0, "shared words -> positive similarity");
        assert!(related < same, "partial overlap < full overlap");
        assert!(unrelated < related, "no shared words -> least similar");
    }
}
