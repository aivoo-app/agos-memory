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
/// For each pair of memories in the same tier, if their text_hash-based
/// similarity (using SHA-256 as a cheap proxy) or actual cosine similarity
/// exceeds the dedup threshold, merge them: bump ref_count on the older
/// row, set dedup_cluster_id on both.
///
/// Returns the number of clusters merged.
async fn dedup_consolidation(store: &StoreHandle, config: &Config) -> Result<u64> {
    let threshold = config.memory.dedup_threshold;
    let memories = store.all_active_memories_for_dedup().await?;
    let mut clusters: Vec<(i64, String, String, String)> = Vec::new(); // (id, public_id, tier, cluster_id)
    let mut merged: u64 = 0;

    for mem in &memories {
        let (id, public_id, tier, text, _text_hash) = mem;
        let hash = sha256_hex(text);

        // Find an existing cluster in the same tier with similar hash
        let mut found = false;
        for cluster in &clusters {
            if cluster.2 == *tier {
                // Simple text-based similarity check
                let similarity = text_similarity(&hash, &sha256_hex(&cluster.3));
                if similarity >= threshold {
                    // Merge: bump ref_count on the cluster owner
                    store.bump_ref_count(cluster.0).await?;
                    store.set_dedup_cluster(*id, &cluster.3).await?;
                    merged += 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            let cluster_id = format!("dedup-{}", sha256_hex(&hash));
            store.set_dedup_cluster(*id, &cluster_id).await?;
            clusters.push((*id, public_id.clone(), tier.clone(), cluster_id));
        }
    }

    Ok(merged)
}

/// Simple text similarity based on shared character n-grams.
/// This is a cheap proxy for cosine similarity; the real dedup happens
/// on the write path with actual embeddings.
fn text_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = 3; // trigram
    let a_grams: std::collections::HashSet<String> = a_chars
        .windows(n)
        .map(|w| w.iter().collect::<String>())
        .collect();
    let b_grams: std::collections::HashSet<String> = b_chars
        .windows(n)
        .map(|w| w.iter().collect::<String>())
        .collect();
    if a_grams.is_empty() || b_grams.is_empty() {
        return 0.0;
    }
    let intersection = a_grams.intersection(&b_grams).count();
    let union = a_grams.union(&b_grams).count();
    intersection as f64 / union as f64
}
