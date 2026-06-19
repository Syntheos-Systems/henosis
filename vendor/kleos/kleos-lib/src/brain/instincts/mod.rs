// brain/instincts/mod.rs -- synthetic pre-training corpus for fresh Engram brains.
//
// Loads ~200 ghost memories from a binary corpus file and seeds them into a
// HopfieldNetwork so a brand-new brain is never a blank slate. Ghosts use
// negative IDs and start at GHOST_STRENGTH = 0.3.
//
// Binary format (version 2):
//   [0..4]  magic  b"INST"
//   [4..8]  version u32 little-endian
//   [8..12] compressed-body length u32 little-endian
//   [12..]  gzip-compressed JSON (InstinctsCorpus)

pub mod types;

pub use types::*;

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::Path;

use crate::brain::hopfield::edges;
use crate::brain::hopfield::network::{self, HopfieldNetwork};
use crate::brain::hopfield::recall;
use crate::brain::hopfield::types::EdgeType;
use crate::db::Database;
use crate::{EngError, Result};

// ---- Constants ----

pub const GHOST_STRENGTH: f32 = 0.3;
pub const GHOST_REPLACE_SIM: f32 = 0.85;

const INST_MAGIC: &[u8; 4] = b"INST";
// Accept versions 1 and 2 -- the bundled file is version 1, the eidolon
// generator source declares version 2 but has not yet regenerated the corpus.
const INST_VERSION_MIN: u32 = 1;
const INST_VERSION_MAX: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReapplyReport {
    pub patterns_added: usize,
    pub patterns_skipped_existing: usize,
    pub edges_rewritten: usize,
}

// ---- Binary loader ----

/// Load an InstinctsCorpus from the binary .bin file.
///
/// Format: 4-byte magic "INST" + 4-byte u32 version + gzip-compressed JSON body.
pub fn load_instincts_bin(path: &Path) -> Result<InstinctsCorpus> {
    let raw = std::fs::read(path)
        .map_err(|e| EngError::Internal(format!("instincts: cannot read {:?}: {}", path, e)))?;

    // Header is: 4-byte magic + 4-byte version u32-LE + 4-byte compressed-length u32-LE
    if raw.len() < 12 {
        return Err(EngError::Internal("instincts: file too short".to_string()));
    }

    // Validate magic header
    if &raw[0..4] != INST_MAGIC {
        return Err(EngError::Internal(
            "instincts: bad magic header (expected INST)".to_string(),
        ));
    }

    // Validate version
    let version = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    if !(INST_VERSION_MIN..=INST_VERSION_MAX).contains(&version) {
        return Err(EngError::Internal(format!(
            "instincts: unsupported version {} (supported {}-{})",
            version, INST_VERSION_MIN, INST_VERSION_MAX
        )));
    }

    // 4-byte compressed-body length (bytes 8..12), then gzip body at offset 12
    let compressed = &raw[12..];
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut json_bytes = Vec::new();
    decoder
        .read_to_end(&mut json_bytes)
        .map_err(|e| EngError::Internal(format!("instincts: gzip decompress failed: {}", e)))?;

    // Parse JSON
    let corpus: InstinctsCorpus = serde_json::from_slice(&json_bytes)
        .map_err(|e| EngError::Internal(format!("instincts: JSON parse failed: {}", e)))?;

    Ok(corpus)
}

// ---- Seeding ----

/// Seed instinct patterns into the Hopfield network and database for a user.
///
/// Loads the corpus from the binary file bundled with the crate. Only seeds
/// if the brain currently has 0 patterns (fresh brain guard). Records
/// completion in brain_meta so re-runs are idempotent.
///
/// Returns the number of patterns actually seeded (0 if already seeded or
/// brain already had patterns).
#[tracing::instrument(skip(db, network), fields(user_id))]
pub async fn seed_instincts(
    db: &Database,
    network: &mut HopfieldNetwork,
    user_id: i64,
) -> Result<usize> {
    // Check if already seeded via brain_meta
    if is_seeded(db, user_id).await? {
        return Ok(0);
    }

    // Only seed a blank brain
    let existing = crate::brain::hopfield::pattern::count_patterns(db, user_id).await?;
    if existing > 0 {
        // Brain already has patterns -- mark seeded to avoid future checks and bail
        mark_seeded(db, user_id).await?;
        return Ok(0);
    }

    // Locate the bundled binary file
    let bin_path = instincts_bin_path();
    if !bin_path.exists() {
        return Err(EngError::Internal(format!(
            "instincts: corpus file not found at {:?}",
            bin_path
        )));
    }

    let corpus = load_instincts_bin(&bin_path)?;
    let count = corpus.memories.len();

    for mem in &corpus.memories {
        if mem.embedding.is_empty() {
            continue;
        }
        recall::store_pattern(
            db,
            network,
            mem.id,
            &mem.embedding,
            user_id,
            mem.importance,
            GHOST_STRENGTH,
        )
        .await?;
    }

    mark_seeded(db, user_id).await?;

    Ok(count)
}

// ---- Internal helpers ----

/// Return the canonical path to the bundled instincts.bin.
/// Resolves relative to CARGO_MANIFEST_DIR at compile time, falls back to
/// a runtime path via the binary's location.
fn instincts_bin_path() -> std::path::PathBuf {
    // Try compile-time manifest dir first (works in tests and dev builds)
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let p = std::path::PathBuf::from(manifest)
            .join("data")
            .join("instincts.bin");
        if p.exists() {
            return p;
        }
    }

    // Runtime fallback: binary dir / data / instincts.bin
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("data").join("instincts.bin");
            if p.exists() {
                return p;
            }
        }
    }

    // Last resort: relative cwd
    std::path::PathBuf::from("data/instincts.bin")
}

async fn is_seeded(db: &Database, user_id: i64) -> Result<bool> {
    let key = format!("instincts_seeded_{}", user_id);
    db.read(move |conn| {
        let mut stmt = conn.prepare("SELECT value FROM brain_meta WHERE key = ?1")?;
        let exists = stmt.exists(rusqlite::params![key])?;
        Ok(exists)
    })
    .await
}

async fn mark_seeded(db: &Database, user_id: i64) -> Result<()> {
    let key = format!("instincts_seeded_{}", user_id);
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    db.write(move |conn| {
        conn.execute(
            "INSERT OR REPLACE INTO brain_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, now],
        )?;
        Ok(())
    })
    .await
}

// ---- Reapply instincts ----

/// Re-apply instinct corpus to a populated brain.
///
/// Unlike `seed_instincts`, this skips the blank-brain guard and the
/// `brain_meta` seeded flag. Patterns whose ID already exists in the
/// network are skipped. Edges from the corpus are always rewritten
/// (upserted) so that new corpus edges reach existing brains.
#[tracing::instrument(skip(db, network), fields(user_id))]
pub async fn reapply_instincts(
    db: &Database,
    network: &mut HopfieldNetwork,
    user_id: i64,
) -> Result<ReapplyReport> {
    let bin_path = instincts_bin_path();
    if !bin_path.exists() {
        return Err(EngError::Internal(format!(
            "instincts: corpus file not found at {:?}",
            bin_path
        )));
    }

    let corpus = load_instincts_bin(&bin_path)?;

    let mut patterns_added = 0usize;
    let mut patterns_skipped = 0usize;

    for mem in &corpus.memories {
        if mem.embedding.is_empty() {
            continue;
        }
        if network.strength(mem.id).is_some() {
            patterns_skipped += 1;
            continue;
        }
        recall::store_pattern(
            db,
            network,
            mem.id,
            &mem.embedding,
            user_id,
            mem.importance,
            GHOST_STRENGTH,
        )
        .await?;
        patterns_added += 1;
    }

    let mut edges_rewritten = 0usize;
    for edge in &corpus.edges {
        let etype = EdgeType::from_str_loose(&edge.edge_type);
        edges::store_edge(
            db,
            edge.source_id,
            edge.target_id,
            edge.weight,
            etype,
            user_id,
        )
        .await?;
        edges_rewritten += 1;
    }

    Ok(ReapplyReport {
        patterns_added,
        patterns_skipped_existing: patterns_skipped,
        edges_rewritten,
    })
}

// ---- Ghost replacement ----

/// Check whether any ghost patterns (negative IDs) in the Hopfield network
/// are sufficiently similar to a newly absorbed real embedding. If so,
/// remove the ghost from both the in-memory network and the database.
///
/// A ghost is replaced when `cosine_sim(new_embedding, ghost_pattern) > GHOST_REPLACE_SIM`.
/// This prevents the corpus from double-representing knowledge that the brain
/// has now learned from real experience.
///
/// Returns the number of ghosts removed.
#[tracing::instrument(skip(db, network, new_embedding), fields(embedding_len = new_embedding.len(), user_id))]
pub async fn check_ghost_replacement(
    db: &Database,
    network: &mut HopfieldNetwork,
    new_embedding: &[f32],
    user_id: i64,
) -> Result<usize> {
    use crate::brain::hopfield::pattern;

    // Only consider negative IDs (ghost patterns) that are in the live network.
    let ghost_ids: Vec<i64> = network
        .pattern_ids()
        .iter()
        .copied()
        .filter(|&id| id < 0)
        .collect();

    if ghost_ids.is_empty() {
        return Ok(0);
    }

    // Load ghost patterns from DB to get their embedding vectors.
    let all_patterns = pattern::list_patterns(db, user_id).await?;
    let ghost_patterns: Vec<_> = all_patterns
        .iter()
        .filter(|p| ghost_ids.contains(&p.id))
        .collect();

    let normalized_new = network::l2_normalize(new_embedding);

    let mut to_remove: Vec<i64> = Vec::new();
    for ghost in ghost_patterns {
        let normalized_ghost = network::l2_normalize(&ghost.pattern);
        let sim = network::cosine_similarity(&normalized_new, &normalized_ghost);
        if sim > GHOST_REPLACE_SIM {
            to_remove.push(ghost.id);
        }
    }

    let count = to_remove.len();
    for id in to_remove {
        network.remove(id);
        if let Err(e) = pattern::delete_pattern(db, id, user_id).await {
            tracing::warn!(pattern_id = id, error = %e, "delete_pattern (ghost replacement) failed");
        }
    }

    Ok(count)
}

// ---- Tests ----

#[cfg(test)]
mod tests;
