//! Token packing (issue 0034, decision D25).
//!
//! Rerank decides *what order*; packing decides *what fits*. They are separate
//! because a ranking is worthless if the assembled context blows the ceiling,
//! and because "fits" is not a property of a single item: tier shares and the
//! global ceiling interact.
//!
//! Three rules, all D25:
//!
//! 1. **Pinned first.** A pinned row is a user instruction, so it claims budget
//!    before any score ordering and is not bound by its tier's share.
//! 2. **Tier shares with rollover.** Each tier is offered `budget · split[tier]`
//!    in declared order (`working → episodic → semantic → procedural`); a share
//!    the tier does not use rolls down to the next tier, so a quiet tier never
//!    wastes budget. Note the consequence: because tiers are served in *declared*
//!    order, not score order, a strong `semantic` memory can be starved by a
//!    weaker `working` one — that is the point of the split, and rollover softens
//!    it whenever an earlier tier is quiet.
//! 3. **Whole-item drop or summary-swap, never truncation.** If the full text
//!    does not fit, the stored `summary_text` is placed instead when *it* fits,
//!    otherwise the item is dropped whole. A truncated memory would inject half a
//!    statement as though it were the whole one.
//!
//! The ceiling is enforced on **every** placement against a running total, so
//! `Σ placed tokens <= budget_tokens` holds however shares, rollover and pinning
//! interact — the shares are a preference, the ceiling is the invariant. That
//! matters because a share can round up: with `budget = 9`, the 40/30/20/10
//! shares round to 4/3/2/1 = 10 tokens in total, one more than the budget.

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::config::BudgetSplit;
use crate::util::TokenCounter;

use super::filter::CanonicalRow;
use super::fuse::{DropReason, Placement, RecallHit};

/// Declared tier order: shares are offered, and leftovers rolled over, in this
/// order (D25).
pub(super) const TIER_ORDER: [&str; 4] = ["working", "episodic", "semantic", "procedural"];

/// Tokens placed per tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TierTokens {
    /// Tokens placed from the `working` tier.
    pub working: u64,
    /// Tokens placed from the `episodic` tier.
    pub episodic: u64,
    /// Tokens placed from the `semantic` tier.
    pub semantic: u64,
    /// Tokens placed from the `procedural` tier.
    pub procedural: u64,
}

impl TierTokens {
    /// Tokens placed from `tier` (`0` for an unknown tier).
    pub fn get(&self, tier: &str) -> u64 {
        match tier {
            "working" => self.working,
            "episodic" => self.episodic,
            "semantic" => self.semantic,
            "procedural" => self.procedural,
            _ => 0,
        }
    }

    /// Add `tokens` to `tier`'s total (a no-op for an unknown tier, which the
    /// SQL `CHECK` constraint makes unreachable).
    fn add(&mut self, tier: &str, tokens: u64) {
        match tier {
            "working" => self.working += tokens,
            "episodic" => self.episodic += tokens,
            "semantic" => self.semantic += tokens,
            "procedural" => self.procedural += tokens,
            _ => {}
        }
    }
}

/// Per-tier shares for `budget`, rounded to the nearest token (D25).
///
/// These are what each tier is *offered*; the sum may exceed `budget` by a token
/// or two after rounding, which is harmless because the running ceiling check
/// below is what actually bounds the result.
pub fn alloc(split: &BudgetSplit, budget: u64) -> TierTokens {
    let share = |f: f64| (budget as f64 * f).round() as u64;
    TierTokens {
        working: share(split.working),
        episodic: share(split.episodic),
        semantic: share(split.semantic),
        procedural: share(split.procedural),
    }
}

/// What packing produced.
pub(super) struct PackOutcome {
    /// Hits in injection order (pinned first), each with its [`Placement`].
    pub hits: Vec<RecallHit>,
    /// Total tokens placed; the caller's guarantee is `<= budget_tokens`.
    pub tokens_used: u64,
    /// Tokens placed per tier.
    pub tier_tokens: TierTokens,
}

/// Cost of an item's summary, when it has a usable one (D25 summary-swap).
///
/// The cost is the *larger* of the writer's recorded `summary_tokens` and the
/// measured count. Both are produced by the same heuristic in practice, so this
/// is a no-op in the normal case; it matters when a writer recorded a smaller
/// figure, because trusting it would break the ceiling guarantee this module
/// exists to provide. A summary that was stored without a recorded count is
/// still usable — it is simply measured.
fn summary_cost(row: &CanonicalRow, counter: &dyn TokenCounter) -> Option<u64> {
    let text = row.summary_text.as_deref()?;
    if text.is_empty() {
        return None;
    }
    let measured = counter.count(text);
    let recorded = row.summary_tokens.unwrap_or(0).max(0) as u64;
    Some(recorded.max(measured))
}

/// Place one item against `cap`, preferring the full text and falling back to
/// the summary (D25).
///
/// `reason` is the drop reason to report when neither fits; the caller decides
/// it from which cap was binding.
fn place(
    row: &CanonicalRow,
    cap: u64,
    counter: &dyn TokenCounter,
    reason: DropReason,
) -> (Placement, u64) {
    let full = counter.count(&row.text);
    if full <= cap {
        return (Placement::Full, full);
    }
    if let Some(summary) = summary_cost(row, counter)
        && summary <= cap
    {
        return (Placement::Summary, summary);
    }
    (Placement::Dropped(reason), 0)
}

/// Pack `hits` into `budget` tokens using `split` for tier shares (D25).
///
/// Returns the hits in injection order with their placements, plus the token
/// totals. Every hit is placed, summarised or dropped — none is left
/// [`Placement::Unpacked`] — so the caller can trust the report's order and
/// totals.
pub(super) fn pack(
    split: &BudgetSplit,
    budget: u64,
    hits: Vec<RecallHit>,
    rows: &HashMap<i64, CanonicalRow>,
    counter: &dyn TokenCounter,
) -> PackOutcome {
    // Injection order: pinned first (first claim on budget), then by rerank
    // score. `public_id` breaks ties so packing — which decides what a user
    // sees — cannot depend on hash iteration order.
    let mut hits = hits;
    hits.sort_by(|a, b| {
        let pinned_a = is_pinned(rows, a);
        let pinned_b = is_pinned(rows, b);
        pinned_b
            .cmp(&pinned_a) // pinned (true) first
            .then_with(|| score_desc(a, b))
            .then_with(|| a.public_id.cmp(&b.public_id))
    });

    let shares = alloc(split, budget);
    let mut tokens_used: u64 = 0;
    let mut tier_tokens = TierTokens::default();

    // ---- pass 1: pinned, ignoring tier shares (but never the ceiling).
    for hit in &mut hits {
        let Some(row) = rows.get(&hit.rowid) else {
            hit.placement = Placement::Dropped(DropReason::Unresolved);
            continue;
        };
        if !row.pinned {
            continue;
        }
        let cap = budget.saturating_sub(tokens_used);
        let (placement, tokens) = place(row, cap, counter, DropReason::TotalBudget);
        apply(
            hit,
            &row.tier,
            placement,
            tokens,
            &mut tokens_used,
            &mut tier_tokens,
        );
    }

    // ---- pass 2: tier shares in declared order, leftovers rolling down.
    let mut carry: u64 = 0;
    for tier in TIER_ORDER {
        let mut available = shares.get(tier).saturating_add(carry);
        for hit in &mut hits {
            if !matches!(hit.placement, Placement::Unpacked) {
                continue;
            }
            // The canonical row's tier is authoritative (fuse copied it, but
            // packing must not depend on that copy being in sync).
            let Some(row) = rows.get(&hit.rowid) else {
                hit.placement = Placement::Dropped(DropReason::Unresolved);
                continue;
            };
            if row.tier != tier {
                continue;
            }
            let by_total = budget.saturating_sub(tokens_used);
            // The binding cap decides why an item was dropped: a tier share can
            // leave budget unspent, and the reason must name that.
            let (cap, reason) = if available <= by_total {
                (available, DropReason::TierBudget)
            } else {
                (by_total, DropReason::TotalBudget)
            };
            let (placement, tokens) = place(row, cap, counter, reason);
            if tokens > 0 {
                available = available.saturating_sub(tokens);
            }
            apply(
                hit,
                &row.tier,
                placement,
                tokens,
                &mut tokens_used,
                &mut tier_tokens,
            );
        }
        carry = available; // unused share rolls to the next tier
    }

    PackOutcome {
        hits,
        tokens_used,
        tier_tokens,
    }
}

/// Whether a hit's canonical row is pinned (`false` when the row is missing).
fn is_pinned(rows: &HashMap<i64, CanonicalRow>, hit: &RecallHit) -> bool {
    rows.get(&hit.rowid).is_some_and(|r| r.pinned)
}

/// Descending score comparison that treats a NaN score as lowest.
fn score_desc(a: &RecallHit, b: &RecallHit) -> Ordering {
    b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal)
}

/// Record a placement on the hit and roll it into the running totals.
fn apply(
    hit: &mut RecallHit,
    tier: &str,
    placement: Placement,
    tokens: u64,
    tokens_used: &mut u64,
    tier_tokens: &mut TierTokens,
) {
    hit.placement = placement;
    hit.tokens = tokens;
    *tokens_used += tokens;
    tier_tokens.add(tier, tokens);
}
