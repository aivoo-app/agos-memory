//! Reciprocal Rank Fusion (D27) of the vec and FTS legs, with a post-fuse
//! re-check of the canonical rows (defense in depth; 0032 extracts this into
//! the shared filter module).

use std::collections::HashMap;

use super::query::RecallQuery;

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

/// Canonical row data resolved for a vec hit: (public_id, tier, status, trust).
pub(super) type CanonicalRow = (String, String, String, String);

/// An FTS-leg hit before fusion: (public_id, tier, trust).
pub(super) type FtsLegHit = (String, String, String);

/// Fuse both legs' already-filtered candidates with RRF and return the top-k
/// hits, best first.
///
/// Every candidate is re-checked against the canonical `memories` row it must
/// have: the vec leg's metadata filters can drift from the canonical row (a
/// stale vec row), so the predicate is applied a second time on resolved data.
/// Nothing may reach scoring that the canonical rows do not vouch for.
pub(super) fn fuse(
    q: &RecallQuery,
    vec_hits: &[VecLegHit],
    rows: &HashMap<i64, CanonicalRow>,
    fts_hits: &[FtsLegHit],
) -> Vec<RecallHit> {
    let tiers = q.tiers();
    let statuses = q.statuses();
    let trusts = q.trusts();

    // public_id -> fused accumulator
    type Acc = (f64, Option<f64>, Option<usize>, String, String);
    let mut acc: HashMap<String, Acc> = HashMap::new();

    // Vec leg: rank is the KNN order (best first).
    for (rank, (rowid, distance)) in vec_hits.iter().enumerate() {
        let Some((public_id, tier, status, trust)) = rows.get(rowid) else {
            continue; // orphaned vector: no canonical row, never surface it
        };
        if !tiers.contains(&tier.as_str())
            || !statuses.contains(&status.as_str())
            || !trusts.contains(&trust.as_str())
        {
            continue; // stale vec metadata: the canonical row vetoes it
        }
        let entry = acc
            .entry(public_id.clone())
            .or_insert_with(|| (0.0, None, None, tier.clone(), trust.clone()));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        entry.1 = Some(1.0 - *distance); // cosine distance -> similarity
    }

    // FTS leg: rank is the BM25 order (best first).
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
    hits.truncate(q.k);
    hits
}
