//! Query parameters for one recall call (defaults from `[recall]` config).

use crate::config::{RecallConfig, TrustPolicy};

/// One recall request (D26/D28/D29 defaults live in [`RecallConfig`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallQuery {
    /// Query text (embedded for the vec leg, tokenized for the FTS leg).
    pub text: String,
    /// Return at most `k` fused hits (D28: default 8).
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
        }
    }

    /// Tier allowlist for this query (D26: episodic opt-in).
    pub(crate) fn tiers(&self) -> &'static [&'static str] {
        if self.include_episodic {
            &["working", "episodic", "semantic", "procedural"]
        } else {
            &["working", "semantic", "procedural"]
        }
    }

    /// Trust allowlist for this query (D29).
    pub(crate) fn trusts(&self) -> &'static [&'static str] {
        match self.trust_policy {
            TrustPolicy::Strict => &["trusted", "system"],
            TrustPolicy::Fenced => &["trusted", "system", "untrusted"],
        }
    }

    /// Status allowlist for this query (D28: pending opt-in).
    pub(crate) fn statuses(&self) -> &'static [&'static str] {
        if self.include_pending {
            &["active", "pending"]
        } else {
            &["active"]
        }
    }
}
