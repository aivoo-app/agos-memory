//! Write path: sessions, turns, jobs, extraction, persistence (v0.2.0).
//!
//! The write path turns conversation turns into durable memories:
//!
//! - [`sessions`] — open/append/close with idle timeout
//! - jobs/worker, extractor, persist/dedup, trust/redact — land in 0026..0029
//! - [`remember`] — the public entry: one fact in, one memory out

pub mod consolidate;
pub mod extract;
pub mod jobs;
pub mod persist;
pub mod redact;
pub mod sessions;
pub mod summarize;
pub mod ttl_reaper;
pub mod worker;

pub use consolidate::run_consolidation_job;
pub use extract::Candidate;
pub use jobs::{JobRow, dead_count, enqueue};
pub use persist::{
    DEDUP_THRESHOLD, PersistReport, persist_candidate, persist_candidate_full,
    persist_candidate_unembedded,
};
pub use redact::{REDACTED, redact};
pub use sessions::{SessionRow, TurnRow};
pub use summarize::{SummarizeReport, run_summarization_job, summarize_by_id, summarize_tier};
pub use ttl_reaper::run_ttl_reaper;

/// Confidence below this → `status='pending'` instead of `active` (D-pending).
pub const PENDING_THRESHOLD_DEFAULT: f64 = 0.4;

/// The public write-path entry (issue 0029): one fact in, one memory out.
///
/// Redacts secrets, embeds, dedups, and persists with provenance
/// (`source_kind` → trust) and confidence gating (`pending_threshold`).
///
/// **Availability (issue 0030):** if the embedding provider is unreachable the
/// fact is still stored — the row is written with `embed_status='failed'` and no
/// `vec_memories` row (FTS5 keyword recall can still find it, and a `reembed`
/// job can fill the vector later). Only a *redaction-independent* storage
/// failure aborts the write. Provider outages must never lose a fact.
#[allow(clippy::too_many_arguments)]
pub async fn remember<E: crate::embed::Embedder>(
    store: &crate::storage::StoreHandle,
    tier: &str,
    kind: &str,
    text: &str,
    source_kind: &str,
    confidence: f64,
    embedder: &E,
    extractor_version: &str,
    pending_threshold: f64,
    dedup_threshold: f64,
) -> crate::error::Result<crate::storage::MemoryRow> {
    let cand = Candidate {
        tier: tier.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
        importance: 0.5,
        confidence,
        session_independent: false,
        source_seq: 0,
    };
    match persist_candidate_full(
        store,
        &cand,
        embedder,
        extractor_version,
        source_kind,
        pending_threshold,
        dedup_threshold,
    )
    .await
    {
        Ok(report) => Ok(report.row),
        // Embedder down ≠ fact lost: store it unembedded for a later reembed.
        Err(crate::error::Error::Embedder(msg)) => {
            tracing::warn!(
                error = %msg,
                "embedding provider unavailable; storing memory without a vector \
                 (embed_status='failed') so the fact is not lost"
            );
            let report = persist::persist_candidate_unembedded(
                store,
                &cand,
                extractor_version,
                source_kind,
                pending_threshold,
            )
            .await?;
            Ok(report.row)
        }
        Err(e) => Err(e),
    }
}

/// Degraded write path: no embedding provider (`provider = none`, decision D4).
///
/// Redacts, dedups by `text_hash`, stores with `embed_status='skipped'` and no
/// vector row — FTS5 keyword recall still finds it (issue 0030).
#[allow(clippy::too_many_arguments)]
pub async fn remember_degraded(
    store: &crate::storage::StoreHandle,
    tier: &str,
    kind: &str,
    text: &str,
    source_kind: &str,
    confidence: f64,
    extractor_version: &str,
    pending_threshold: f64,
) -> crate::error::Result<crate::storage::MemoryRow> {
    let cand = Candidate {
        tier: tier.to_string(),
        kind: kind.to_string(),
        text: text.to_string(),
        importance: 0.5,
        confidence,
        session_independent: false,
        source_seq: 0,
    };
    let report = persist::persist_candidate_degraded(
        store,
        &cand,
        extractor_version,
        source_kind,
        pending_threshold,
    )
    .await?;
    Ok(report.row)
}
