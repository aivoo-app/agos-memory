//! Consolidation job (issue 0045).
//!
//! Background job that performs:
//! 1. **Summarization pass**: finds memories older than `summarize_after_days`
//!    without `summary_text`, generates summary via LLM, stores it.
//! 2. **Dedup consolidation**: re-runs cosine dedup across all tiers, merges
//!    clusters, bumps `ref_count`.
//! 3. **Orphan cleanup**: finds `memory_versions` not referenced by any
//!    `memories` row, deletes them.

use crate::config::Config;
use crate::embed::hash_to_unit_vector;
use crate::error::Result;
use crate::llm::ChatClient;
use crate::storage::StoreHandle;
use crate::util::hash::sha256_hex;
use std::sync::Arc;

/// Outcome of a consolidation run.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidateReport {
    /// Number of summaries generated.
    pub summaries_generated: u64,
    /// Number of dedup clusters merged.
    pub dedup_clusters_merged: u64,
    /// Number of orphaned version rows deleted.
    pub orphans_deleted: u64,
}

/// Run the full consolidation job.
///
/// Idempotent: re-running does nothing if everything is already consolidated.
pub async fn run_consolidation_job(
    store: &StoreHandle,
    llm: &Arc<dyn ChatClient>,
    config: &Config,
) -> Result<ConsolidateReport> {
    let mut report = ConsolidateReport {
        summaries_generated: 0,
        dedup_clusters_merged: 0,
        orphans_deleted: 0,
    };

    // 1. Summarization pass — reuse the summarize module
    let total_summaries = super::summarize::run_summarization_job(store, llm, config).await?;
    report.summaries_generated = total_summaries;

    // 2. Dedup consolidation
    let deduped = dedup_consolidation(store, config).await?;
    report.dedup_clusters_merged = deduped;

    // 3. Orphan cleanup
    let orphans = store.delete_orphaned_versions().await?;
    report.orphans_deleted = orphans as u64;

    Ok(report)
}

/// Re-run cosine dedup across all active memories.
///
/// For each pair of memories in the same tier, computes the cosine similarity
/// of their **stored** `vec_memories` embeddings (issue 0056) — the same
/// vectors the write path (`persist.rs` KNN) and recall use. Memories without
/// a stored vector (degraded writes, `provider='none'`) fall back to the
/// deterministic `hash_to_unit_vector` proxy over the text, so consolidation
/// still merges verbatim duplicates offline. When similarity meets or exceeds
/// `memory.dedup_threshold`, merges the cluster: bumps `ref_count` on the
/// cluster owner and tags the newcomer with the cluster id.
///
/// Returns the number of clusters merged.
async fn dedup_consolidation(store: &StoreHandle, config: &Config) -> Result<u64> {
    let threshold = config.memory.dedup_threshold;
    let dim = store.embed_dim().await?;
    let memories = store.all_active_memories_for_dedup().await?;

    // Prefer the stored embedding; fall back to the deterministic text proxy.
    let mut mem_data: Vec<(i64, String, String, Vec<f32>)> = Vec::with_capacity(memories.len());
    for (id, public_id, tier, text, _text_hash, embedding) in &memories {
        let vec = embedding
            .as_deref()
            .and_then(|blob| decode_f32_blob(blob, dim))
            .unwrap_or_else(|| hash_to_unit_vector(text, dim));
        mem_data.push((*id, public_id.clone(), tier.clone(), vec));
    }

    // Cluster bookkeeping: (cluster_id, owner_id, owner_vec, tier).
    let mut clusters: Vec<(String, i64, Vec<f32>, String)> = Vec::new();
    let mut merged: u64 = 0;

    for (id, public_id, tier, vec) in &mem_data {
        let mut found = false;
        for (cluster_id, owner_id, owner_vec, cluster_tier) in &clusters {
            if *cluster_tier == *tier {
                let sim = cosine_similarity(vec, owner_vec);
                if sim >= threshold {
                    store.bump_ref_count(*owner_id).await?;
                    store.set_dedup_cluster(*id, cluster_id).await?;
                    merged += 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            let cluster_id = format!("dedup-{}", sha256_hex(public_id));
            store.set_dedup_cluster(*id, &cluster_id).await?;
            clusters.push((cluster_id, *id, vec.clone(), tier.clone()));
        }
    }

    Ok(merged)
}

/// Decode a `vec_memories` embedding blob (little-endian f32s) into a vector.
///
/// Returns `None` when the blob's length doesn't match `dim` — the row is then
/// treated as vectorless and dedup falls back to the text proxy.
fn decode_f32_blob(blob: &[u8], dim: usize) -> Option<Vec<f32>> {
    if blob.len() != dim * 4 {
        return None;
    }
    Some(
        blob.chunks(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// Cosine similarity of two unit vectors (dot product, clamped to [-1, 1]).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    debug_assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (*x as f64) * (*y as f64))
        .sum::<f64>()
        .clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![-1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - (-1.0_f64)).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b)).abs() < 1e-6);
    }

    #[test]
    fn decode_f32_blob_roundtrip() {
        let v = vec![0.5_f32, -1.25, 2.0, 0.0];
        let blob: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
        assert_eq!(decode_f32_blob(&blob, 4).unwrap(), v);
        // Wrong length → treated as vectorless (fallback path).
        assert!(decode_f32_blob(&blob, 8).is_none());
        assert!(decode_f32_blob(&blob[..6], 1).is_none());
    }
}
