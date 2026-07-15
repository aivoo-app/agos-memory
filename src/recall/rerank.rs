//! Rerank + per-tier decay (issue 0033, decisions D23/D24/D25/D28).
//!
//! Fusion ([`super::fuse`]) answers "which candidates did the two legs agree
//! on"; rerank answers "which of them does the user actually want". The score is
//! the D23 blend:
//!
//! ```text
//! score = w_sim · sim_norm + w_importance · importance_eff + w_decay · decay(t)
//! ```
//!
//! Three things are easy to get wrong here, so each is spelled out:
//!
//! - **`sim_norm` must be a rank term, not a distance.** The vec leg's cosine
//!   similarity and the FTS leg's BM25 score are not on a common scale, which is
//!   exactly why D27 fuses by *rank*. So the similarity term is the RRF score
//!   normalized by its ceiling (both legs at rank 0 → `2/(K+1)`), which is a
//!   `[0,1]` quantity comparable with the other two terms. The raw cosine
//!   similarity is still carried in `components.sim` for explain (0035).
//! - **`confidence` only multiplies `pending` importance.** A `pending` row is
//!   unverified, so its importance is discounted by extraction confidence
//!   (D25/D28). Applying that factor to `active` rows would demote trustworthy
//!   memories for no reason.
//! - **Decay is measured from `COALESCE(last_referenced_at, created_at)`.** A
//!   memory that was used yesterday is fresh even if it was written a year ago;
//!   decaying from `created_at` alone would bury exactly the memories that keep
//!   proving useful.
//!
//! - **Pinned rows ignore age.** A pin is an explicit user instruction, so its
//!   decay term is `1.0`; packing still gives it first claim on the budget.
//!
//! `now` is threaded in from the [`HardFilter`] that already admitted these rows,
//! so the instant used for expiry and the instant used for decay are the same
//! one — otherwise a row could be admitted as unexpired and then scored as
//! infinitely old.

use std::collections::HashMap;

use crate::config::{RecallHalfLives, RecallWeights};

use super::filter::CanonicalRow;
use super::fuse::{RRF_K, RecallHit};

/// RRF ceiling for one query: both legs place the same row at rank 0.
pub(super) const RRF_MAX: f64 = 2.0 / (RRF_K + 1.0);

/// Candidate pool multiplier over `top_k`.
///
/// Each leg must retrieve more than `top_k` so rerank has something to
/// reorder: a row the legs rank 20th can deserve the top slot once decay and
/// importance are accounted for, and it would be unreachable if the legs
/// stopped at `top_k`. Truncation to `top_k` happens *after* rerank.
pub(super) const RERANK_POOL_FACTOR: usize = 4;

/// Half-life of `tier` in minutes; `<= 0` means "no decay" (D24).
///
/// `semantic`/`procedural` default to `0` (∞): stable knowledge should not decay
/// just for being old, while `working`/`episodic` capture time-bound context.
pub fn half_life_minutes(tier: &str, hl: &RecallHalfLives) -> f64 {
    match tier {
        "working" => hl.working_hours * 60.0,
        "episodic" => hl.episodic_hours * 60.0,
        "semantic" => hl.semantic_hours * 60.0,
        "procedural" => hl.procedural_hours * 60.0,
        // Unknown tier: fail soft (no decay) rather than error. Tiers are
        // CHECK-constrained in SQL, so this cannot happen for a resolved row.
        _ => 0.0,
    }
}

/// Exponential decay `0.5^(Δt / half_life)` (D24), in `(0,1]`.
///
/// `Δt` is measured from `last_referenced_at` when the row has one, else from
/// `created_at`; a clock that ran backwards (negative `Δt`) clamps to `1.0`
/// rather than growing above it. A `<= 0` half-life is treated as ∞ (no decay).
pub fn decay(tier: &str, hl: &RecallHalfLives, row: &CanonicalRow, now: i64) -> f64 {
    let hl_min = half_life_minutes(tier, hl);
    if hl_min <= 0.0 {
        return 1.0; // infinite half-life: never decays
    }
    let base = row.last_referenced_at.unwrap_or(row.created_at);
    let delta_min = ((now - base) as f64 / 60_000.0).max(0.0);
    0.5f64.powf(delta_min / hl_min)
}

/// Effective importance for the score: discounted by confidence when the row is
/// still `pending` (D25/D28).
fn importance_eff(row: &CanonicalRow) -> f64 {
    if row.status == "pending" {
        row.importance_current * row.confidence
    } else {
        row.importance_current
    }
}

/// Score and sort fused candidates (D23), best first.
///
/// Truncation to `top_k` and the `min_score` no-hit cut are the caller's job:
/// both must apply to the *reranked* order, and the cut can legitimately return
/// fewer than `k` hits (D26).
///
/// Every candidate's canonical row is looked up in `rows` (the same map the
/// filter produced); a candidate whose row is somehow absent is dropped rather
/// than scored blind — the hard filter, not this function, decides membership,
/// and dropping keeps that invariant intact.
pub(super) fn rerank(
    weights: &RecallWeights,
    half_life: &RecallHalfLives,
    now: i64,
    hits: Vec<RecallHit>,
    rows: &HashMap<i64, CanonicalRow>,
) -> Vec<RecallHit> {
    let mut scored: Vec<RecallHit> = hits
        .into_iter()
        .filter_map(|mut hit| {
            let row = rows.get(&hit.rowid)?;
            let sim_norm = (hit.components.rrf / RRF_MAX).clamp(0.0, 1.0);
            // Decay is keyed off the row's own tier, not the hit's copy of it:
            // the canonical row is authoritative. Pinned rows are explicit user
            // instructions, so age must not lower their score.
            let decay = if row.pinned {
                1.0
            } else {
                decay(&row.tier, half_life, row, now)
            };
            let importance = importance_eff(row);
            let score = f64::from(weights.sim) * sim_norm
                + f64::from(weights.importance) * importance
                + f64::from(weights.decay) * decay;
            hit.score = score;
            hit.components.sim_norm = sim_norm;
            hit.components.importance = importance;
            hit.components.confidence = row.confidence;
            hit.components.decay = decay;
            Some(hit)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.public_id.cmp(&b.public_id)) // deterministic ties
    });
    scored
}
