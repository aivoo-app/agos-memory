//! Write path: sessions, turns, jobs, extraction, persistence (v0.2.0).
//!
//! The write path turns conversation turns into durable memories:
//!
//! - [`sessions`] — open/append/close with idle timeout
//! - jobs/worker, extractor, persist/dedup, trust/redact — land in 0026..0029
//! - [`remember`] — the public entry: one fact in, one memory out

pub mod extract;
pub mod jobs;
pub mod persist;
pub mod redact;
pub mod sessions;
pub mod worker;

pub use extract::Candidate;
pub use jobs::{JobRow, dead_count, enqueue};
pub use persist::{DEDUP_THRESHOLD, PersistReport, persist_candidate, persist_candidate_full};
pub use sessions::{SessionRow, TurnRow};

/// Confidence below this → `status='pending'` instead of `active` (D-pending).
pub const PENDING_THRESHOLD_DEFAULT: f64 = 0.4;

/// The public write-path entry (issue 0029): one fact in, one memory out.
///
/// Redacts secrets, embeds, dedups, and persists with provenance
/// (`source_kind` → trust) and confidence gating (`pending_threshold`).
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
    let report = persist_candidate_full(
        store,
        &cand,
        embedder,
        extractor_version,
        source_kind,
        pending_threshold,
    )
    .await?;
    Ok(report.row)
}
