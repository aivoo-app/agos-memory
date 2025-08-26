//! Reciprocal Rank Fusion (D27) of the vec and FTS legs.
//!
//! Both legs hand in candidates the hard filter already admitted; every
//! candidate — from *either* leg — is matched against the `CanonicalRow` the
//! filter loaded for it, and the filter's Rust mirror ([`HardFilter::admits`])
//! is the gate that decides scoring. This matters for the vec leg because its
//! `vec0` metadata can drift from the canonical row.
//!
//! Candidates are keyed by **rowid**, not `public_id`: the rowid is what the
//! legs and the canonical rows agree on, and it lets rerank (0033) reach the
//! row it needs without a second lookup by an external id.
//!
//! Fusion deliberately does **not** truncate to `top_k`. Rerank reorders by a
//! different score than RRF, so a candidate the legs rank 20th can still earn a
//! top slot once importance and decay are accounted for; truncating here would
//! make it unreachable. The caller retrieves a pool (`top_k ×
//! RERANK_POOL_FACTOR`) and truncates after rerank.

use std::collections::HashMap;

use super::filter::{CanonicalRow, HardFilter};

/// RRF constant (D27).
pub(super) const RRF_K: f64 = 60.0;

/// Score decomposition carried through to rerank/explain (0033/0035).
///
/// The fusion terms (`sim`, `bm25_rank`, `rrf`) are filled by [`fuse`]; the
/// rerank terms (`sim_norm`, `importance`, `confidence`, `decay`) are filled by
/// [`super::rerank`]. Both live here so explain (0035) can print a hit's full
/// provenance without re-deriving anything.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecallComponents {
    /// Cosine similarity from the vec leg (`None` when not a vec hit).
    pub sim: Option<f64>,
    /// 0-based rank within the FTS leg (`None` when not a keyword hit).
    pub bm25_rank: Option<usize>,
    /// Fused RRF score (sum of both legs' `1/(K + rank + 1)`).
    pub rrf: f64,
    /// RRF normalized by its ceiling — the scale-free `sim` term of D23.
    pub sim_norm: f64,
    /// Effective importance used in the score (confidence-discounted when
    /// `pending`).
    pub importance: f64,
    /// Raw `confidence` of the row (diagnostic; only `pending` rows are
    /// discounted by it).
    pub confidence: f64,
    /// Per-tier decay factor in `(0,1]` (D24).
    pub decay: f64,
}

/// One candidate hit, post-fusion.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    /// Canonical row id — the join key between legs and rerank inputs.
    pub rowid: i64,
    /// Public (external) id of the memory row.
    pub public_id: String,
    /// Tier of the memory.
    pub tier: String,
    /// Trust of the memory (D29: untrusted hits must be fenced by the caller).
    pub trust: String,
    /// Final rerank score (D23); `0.0` until [`super::rerank`] runs.
    pub score: f64,
    /// Score components for rerank + explain.
    pub components: RecallComponents,
}

/// Outcome of one recall call.
#[derive(Debug, Clone)]
pub struct RecallReport {
    /// Hits, best first by final score (D23).
    pub hits: Vec<RecallHit>,
    /// True when the vec leg was skipped (embedder unavailable) and the
    /// result is BM25-only.
    pub degraded: bool,
    /// Wall-clock latency in milliseconds.
    pub latency_ms: u64,
}

/// A vec-leg hit before id resolution: (rowid, cosine distance).
pub(super) type VecLegHit = (i64, f64);

/// An FTS-leg hit before fusion: (rowid, public_id, tier, trust).
pub(super) type FtsLegHit = (i64, String, String, String);

/// Fuse both legs' already-filtered candidates with RRF, best first.
///
/// `rows` holds the filter-loaded canonical row for every surviving rowid from
/// either leg. A rowid absent from `rows` was an orphaned vector, a row the
/// filter rejected, or a stale `vec0` metadata row — it never reaches scoring.
pub(super) fn fuse(
    filter: &HardFilter,
    vec_hits: &[VecLegHit],
    rows: &HashMap<i64, CanonicalRow>,
    fts_hits: &[FtsLegHit],
) -> Vec<RecallHit> {
    // rowid -> accumulator
    type Acc = (f64, Option<f64>, Option<usize>);
    let mut acc: HashMap<i64, Acc> = HashMap::new();

    // Vec leg: rank is the KNN order (best first).
    for (rank, (rowid, distance)) in vec_hits.iter().enumerate() {
        let Some(row) = rows.get(rowid) else {
            continue; // orphaned vector, or rejected by the hard filter
        };
        if !filter.admits(row) {
            continue; // stale vec0 metadata: the canonical row vetoes it
        }
        let entry = acc.entry(*rowid).or_insert((0.0, None, None));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        entry.1 = Some(1.0 - *distance); // cosine distance -> similarity
    }

    // FTS leg: rank is the BM25 order (best first). These rows passed the
    // predicate inside the SQL join, and gate 2 re-verified them; the mirror
    // is applied here too so a disagreeing row fails closed.
    for (rank, (rowid, _, _, _)) in fts_hits.iter().enumerate() {
        let Some(row) = rows.get(rowid) else {
            continue; // filter rejected it after the join
        };
        if !filter.admits(row) {
            continue;
        }
        let entry = acc.entry(*rowid).or_insert((0.0, None, None));
        entry.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        entry.2 = Some(rank);
    }

    let mut hits: Vec<RecallHit> = acc
        .into_iter()
        .filter_map(|(rowid, (rrf, sim, bm25_rank))| {
            let row = rows.get(&rowid)?;
            Some(RecallHit {
                rowid,
                public_id: row.public_id.clone(),
                tier: row.tier.clone(),
                trust: row.trust.clone(),
                score: 0.0, // set by rerank (0033)
                components: RecallComponents {
                    sim,
                    bm25_rank,
                    rrf,
                    ..Default::default()
                },
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.components
            .rrf
            .partial_cmp(&a.components.rrf)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.public_id.cmp(&b.public_id)) // deterministic ties
    });
    hits
}
