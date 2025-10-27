//! Recall audit writes (issue 0036).
//!
//! Every `recall()` call writes three linked rows so recall quality is measured
//! over time, not asserted:
//! - 1 `recalls` row: query_hash, query_text, k, budget, candidates, injected,
//!   dropped, top_score, no_hit, latency_ms, session_id, created_at.
//! - N `recall_items` rows: recall_id, memory_id, rank, score, components_json,
//!   injected.
//! - 1 `token_ledger` row: session_id, budget, tokens_used, tier_split_json,
//!   items_injected, items_dropped, created_at.
//!
//! `session_id` is nullable (default `None`) so the CLI recall path works
//! sessionless. Audit writes happen on the writer thread (single writer) and
//! are best-effort: a failure here is logged but never fails the recall call
//! itself, so read quality is never hostage to the audit table.

use crate::error::{Error, Result};
use crate::recall::{Placement, RecallReport, TierTokens};
use crate::storage::StoreHandle;
use crate::util::sha256_hex;

/// Parameters needed to write one recall's audit rows.
///
/// `query_text` is the raw query (stored hashed — `query_hash`); `budget` is
/// `RecallQuery::budget_tokens`; `session_id` is `None` for sessionless calls.
pub struct RecallAuditInput<'a> {
    /// Query text (hashed before storage).
    pub query_text: &'a str,
    /// `k` requested by the query.
    pub k: usize,
    /// Token budget (`budget_tokens`).
    pub budget: u64,
    /// Number of candidates that reached scoring (before packing cut).
    pub candidates: usize,
    /// Nullable session id owning this recall.
    pub session_id: Option<i64>,
    /// Whether the vec leg was unavailable (D4 degraded).
    pub degraded: bool,
}

/// Audit-write outcome: the `recalls.id` of the row written.
pub struct RecallAuditOut {
    /// Primary key of the inserted `recalls` row.
    pub recall_id: i64,
}

/// One hit's audit row payload.
struct RecallAuditItem {
    public_id: String,
    score: f64,
    components_json: String,
    injected: bool,
}

/// Wrap a rusqlite/SQLite error as an audit-write storage error.
fn audit_err<E: std::fmt::Display>(e: E) -> Error {
    Error::Storage(format!("audit write: {e}"))
}

impl StoreHandle {
    /// Write the audit trail for one recall call: `recalls`, `recall_items`,
    /// and `token_ledger`. Best-effort: on error, logs a warning and returns
    /// `Ok(None)` so the recall result is still usable.
    pub async fn write_recall_audit(
        &self,
        report: &RecallReport,
        input: &RecallAuditInput<'_>,
        now_millis: i64,
    ) -> Result<Option<RecallAuditOut>> {
        let query_hash = sha256_hex(input.query_text);
        let k = input.k as i64;
        let budget = input.budget as i64;
        let candidates = input.candidates as i64;
        let injected = report.hits.iter().filter(|h| h.injected()).count() as i64;
        let dropped = report
            .hits
            .iter()
            .filter(|h| matches!(h.placement, Placement::Dropped(_)))
            .count() as i64;
        let top_score: Option<f64> = if report.hits.is_empty() {
            None
        } else {
            let max = report.hits.iter().map(|h| h.score).fold(f64::NAN, f64::max);
            if max.is_nan() { None } else { Some(max) }
        };
        let no_hit = report.no_hit as i64;
        let latency_ms = report.latency_ms as i64;
        let query_text = input.query_text.to_string();
        let session_id: Option<i64> = input.session_id;
        let tier_tokens: &TierTokens = &report.tier_tokens;
        let tier_split_json =
            serde_json::to_string(tier_tokens).unwrap_or_else(|_| "{}".to_string());
        let tokens_used = report.tokens_used as i64;
        let _degraded = input.degraded;

        // Serialize each hit's components_json on the async side (CPU-bound but
        // small; done before handing the closure to spawn_blocking).
        let hits: Vec<RecallAuditItem> = report
            .hits
            .iter()
            .map(|h| RecallAuditItem {
                public_id: h.public_id.clone(),
                score: h.score,
                components_json: serde_json::to_string(&h.components).unwrap_or_default(),
                injected: h.injected(),
            })
            .collect();

        self.write(move |conn| {
            let tx = conn.unchecked_transaction().map_err(audit_err)?;

            // 1. recalls row
            let recall_id: i64 = tx
                .query_row(
                    "INSERT INTO recalls(query_hash, query_text, k, budget, candidates, \
                     injected, dropped, top_score, no_hit, latency_ms, \
                     session_id, created_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                     RETURNING id",
                    rusqlite::params![
                        &query_hash,
                        &query_text,
                        k,
                        budget,
                        candidates,
                        injected,
                        dropped,
                        top_score,
                        no_hit,
                        latency_ms,
                        session_id,
                        now_millis,
                    ],
                    |r| r.get::<_, i64>(0),
                )
                .map_err(audit_err)?;

            // 2. recall_items rows — resolve memory_id by public_id within
            //    the same transaction. A missing memory (GC'd between recall
            //    and audit) is silently skipped. The lookup statement is
            //    prepared with execute_many to avoid borrowing `tx` while we
            //    also need it for INSERTs.
            for (rank, item) in hits.iter().enumerate() {
                let mem_id: i64 = match tx.query_row(
                    "SELECT id FROM memories WHERE public_id = ?1",
                    [&item.public_id],
                    |r| r.get(0),
                ) {
                    Ok(id) => id,
                    Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                    Err(e) => return Err(audit_err(e)),
                };
                tx.execute(
                    "INSERT INTO recall_items(recall_id, memory_id, rank, score, \
                     components_json, injected) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        recall_id,
                        mem_id,
                        rank as i64,
                        item.score,
                        &item.components_json,
                        item.injected as i64,
                    ],
                )
                .map_err(audit_err)?;
            }

            // 3. token_ledger row
            tx.execute(
                "INSERT INTO token_ledger(session_id, budget, tokens_used, \
                 tier_split_json, items_injected, items_dropped, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    session_id,
                    budget,
                    tokens_used,
                    &tier_split_json,
                    injected,
                    dropped,
                    now_millis,
                ],
            )
            .map_err(audit_err)?;

            tx.commit().map_err(audit_err)?;
            Ok(Some(RecallAuditOut { recall_id }))
        })
        .await
    }
}
