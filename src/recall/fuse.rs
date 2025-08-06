//! Reciprocal Rank Fusion (D27) of the vec and FTS legs.
//!
//! Both legs hand in candidates the hard filter already admitted; every vec
//! candidate is additionally matched against the `CanonicalRow` the filter
//! loaded for it, and the filter's Rust mirror ([`HardFilter::admits`]) is the
//! gate that decides scoring — not the vec0 metadata, which can be stale.

use std::collections::HashMap;

use super::filter::{CanonicalRow, HardFilter};

/// RRF constant (D27).
pub(super) const RRF_K: f64 = 60.0;

/// Score decomposition carried through to rerank/explain (0033/0035).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallComponents {
    /// Cosine similarity from the vec leg (`None` when not a vec hit).
    pub sim: Option<f64>,
    /// 0-based rank within the FTS leg (`None` when not a keyword hit).
    pub bm25_rank: Option<usize>,
    /// Fused RRF score.
    pub rrf: f64,
}

/// One fused hit. Rerank/packing (0033/0034) refine this further.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    /// Public (external) id of the memory row.
    pub public_id: String,
    /// Tier of the memory.
    pub tier: String,
    /// Trust of the memory (D29: untrusted hits must be fenced by the caller).
    pub trust: String,
    /// Score components for rerank + explain.
    pub components: RecallComponents,
}

/// Outcome of one recall call.
#[derive(Debug, Clone)]
pub struct RecallReport {
    /// Fused hits, best first (before rerank/packing).
    pub hits: Vec<RecallHit>,
    /// True when the vec leg was skipped (embedder unavailable) and the
    /// result is BM25-only.
    pub degraded: bool,
    /// Wall-clock latency in milliseconds.
    pub latency_ms: u64,
}

/// A vec-leg hit before id resolution: (rowid, cosine distance).
pub(super) type VecLegHit = (i64, f64);

/// An FTS-leg hit before fusion: (public_id, tier, trust).
pub(super) type FtsLegHit = (String, String, String);

/// Fuse both legs' already-filtered candidates with RRF and return the top-k
/// hits, best first.
///
/// `rows` holds the filter-loaded canonical row for each surviving vec rowid.
/// A rowid absent from `rows` was either an orphaned vector or a row the filter
/// rejected — it can never reach scoring.
pub(super) fn fuse(
    filter: &HardFilter,
    k: usize,
    vec_hits: &[VecLegHit],
    rows: &HashMap<i64, CanonicalRow>,
    fts_hits: &[FtsLegHit],
) -> Vec<RecallHit> {
    // public_id -> fused accumulator
    type Acc = (f64, Option<f64>, Option<usize>, String, String);
    let mut acc: HashMap<String, Acc> = HashMap::new();

    // Vec leg: rank is the KNN order (best first).
    for (rank, (rowid, distance)) in vec_hits.iter().enumerate() {
        let Some(row) = rows.get(rowid) else {
            continue; // orphaned vector, or rejected by the hard filter
        };
        if !filter.admits(row) {
            continue; // stale vec0 metadata: the canonical row vetoes it
        }
        let entry = acc
            .entry(row.public_id.clone())
            .or_insert_with(|| (0.0, None, None, row.tier.clone(), row.trust.clone()));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        entry.1 = Some(1.0 - *distance); // cosine distance -> similarity
    }

    // FTS leg: rank is the BM25 order (best first). These rows already passed
    // the predicate inside the SQL join.
    for (rank, (public_id, tier, trust)) in fts_hits.iter().enumerate() {
        let entry = acc
            .entry(public_id.clone())
            .or_insert_with(|| (0.0, None, None, tier.clone(), trust.clone()));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        entry.2 = Some(rank);
    }

    let mut hits: Vec<RecallHit> = acc
        .into_iter()
        .map(
            |(public_id, (rrf, sim, bm25_rank, tier, trust))| RecallHit {
                public_id,
                tier,
                trust,
                components: RecallComponents {
                    sim,
                    bm25_rank,
                    rrf,
                },
            },
        )
        .collect();
    hits.sort_by(|a, b| {
        b.components
            .rrf
            .partial_cmp(&a.components.rrf)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.public_id.cmp(&b.public_id)) // deterministic ties
    });
    hits.truncate(k);
    hits
}
