//! Query parameters for one recall call (defaults from `[recall]` config).

use crate::config::{RecallConfig, RecallHalfLives, RecallWeights, TrustPolicy};

/// One recall request (D23/D26/D28/D29 defaults live in [`RecallConfig`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallQuery {
    /// Query text (embedded for the vec leg, tokenized for the FTS leg).
    pub text: String,
    /// Return at most `k` hits (D28: default 8).
    pub k: usize,
    /// Soft token ceiling for packing — recorded here, enforced by 0034.
    pub budget_tokens: u64,
    /// Include `episodic`-tier memories (D26: default off).
    pub include_episodic: bool,
    /// Include `pending` memories (D28: default off).
    pub include_pending: bool,
    /// Trust handling (D29: Strict excludes `untrusted`; Fenced retrieves it
    /// and the caller renders it inside a data fence).
    pub trust_policy: TrustPolicy,
    /// Minimum rerank score for a hit (D26: below this, report no hit).
    pub min_score: f64,
    /// Rerank weight split (D23).
    pub weights: RecallWeights,
    /// Per-tier decay half-lives (D24).
    pub half_life: RecallHalfLives,
}

impl RecallQuery {
    /// A query with the configured defaults applied.
    pub fn new(text: impl Into<String>, cfg: &RecallConfig) -> Self {
        Self {
            text: text.into(),
            k: cfg.top_k,
            budget_tokens: cfg.budget_tokens,
            include_episodic: cfg.include_episodic,
            include_pending: cfg.include_pending,
            trust_policy: cfg.trust_policy.clone(),
            min_score: f64::from(cfg.min_score),
            weights: cfg.weights.clone(),
            half_life: cfg.half_life.clone(),
        }
    }
}

// Eligible tiers/statuses/trusts live in `crate::recall::filter`, which is the
// single source of truth for hard-filter membership — deliberately not
// duplicated here.
