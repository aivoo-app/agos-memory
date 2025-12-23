//! TTL reaper (issues 0044, 0047).
//!
//! Background job that runs daily at the configured hour (UTC):
//! 1. Finds active memories whose TTL has expired.
//! 2. Soft-deprecates them (status = 'deprecated').
//! 3. After the grace period, hard-purges them.
//!
//! Every action is logged to `forget_audit`.

use crate::config::Config;
use crate::error::Result;
use crate::storage::StoreHandle;
use crate::util::{Clock, SystemClock};

/// Outcome of a TTL reaper run.
#[derive(Debug, Clone, PartialEq)]
pub struct ReaperReport {
    /// Number of memories soft-deprecated due to TTL expiry.
    pub soft_deprecated: u64,
    /// Number of memories hard-purged after grace period.
    pub hard_purged: u64,
}

/// Run the TTL reaper.
///
/// 1. Finds active memories past their per-tier TTL → soft-deprecate.
/// 2. Finds deprecated memories past the grace period → hard-purge.
///
/// Idempotent: re-running is safe.
pub async fn run_ttl_reaper(store: &StoreHandle, config: &Config) -> Result<ReaperReport> {
    let mut report = ReaperReport {
        soft_deprecated: 0,
        hard_purged: 0,
    };

    let now = SystemClock.now_millis();
    let ttl_working = config.memory.ttl_for_tier("working") as i64;
    let ttl_episodic = config.memory.ttl_for_tier("episodic") as i64;
    let ttl_semantic = config.memory.ttl_for_tier("semantic") as i64;
    let ttl_procedural = config.memory.ttl_for_tier("procedural") as i64;

    // 1. Find and soft-deprecate expired active memories
    let expired = store
        .expired_memories(now, ttl_working, ttl_episodic, ttl_semantic, ttl_procedural)
        .await?;

    for (memory_id, _public_id, _tier) in &expired {
        // `ttl_deprecate` audit action (issue 0054): automated retention
        // actions are distinguishable from manual `deprecate` in the ledger.
        store.deprecate_for_ttl(*memory_id).await?;
        report.soft_deprecated += 1;
    }

    // 2. Find and hard-purge deprecated memories past the grace period
    let grace_millis = config.memory.grace_days() as i64 * 24 * 60 * 60 * 1000;
    let past_grace = store.deprecated_past_grace(grace_millis).await?;

    for (memory_id, _public_id) in &past_grace {
        // `ttl_purge` audit action (issue 0054), verified purge with real
        // VACUUM + fail-closed row-count verification.
        store.hard_purge_for_ttl(*memory_id).await?;
        report.hard_purged += 1;
    }

    Ok(report)
}
