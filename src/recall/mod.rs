//! Hybrid recall (issue 0031, decisions D26/D27/D28/D29).
//!
//! Two candidate legs, fused after hard filtering:
//!
//! - **Vector leg**: sqlite-vec KNN over `vec_memories` with metadata filters
//!   (`status`, `trust`, `tier`) pushed *inside* the scan so dead rows never
//!   occupy top-k slots.
//! - **Keyword leg**: FTS5 BM25 over `fts_memories` joined to `memories` for
//!   the same predicate (the FTS index carries no trust/status columns).
//!
//! Fusion is Reciprocal Rank Fusion (D27, k=60) — rank-based, so the two legs'
//! scores never need calibrating. The merged id set is hard-filtered *again*
//! against the canonical `memories` rows before returning (defense in depth;
//! 0032 will extract that predicate into the shared filter module).
//!
//! Degraded mode (D4): when the embedder refuses (no provider / ceiling), the
//! vector leg is skipped, BM25-only results are returned, and
//! [`RecallReport::degraded`] is set — never a hard failure.

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::storage::StoreHandle;
use crate::util::SystemClock;
use crate::util::clock::Clock;

/// Vec0 metadata code for `status` (CHECK order in the schema).
const STATUS_ACTIVE: i64 = 0;
const STATUS_PENDING: i64 = 1;

/// Vec0 metadata code for `trust` (CHECK order in the schema).
const TRUST_TRUSTED: i64 = 0;
const TRUST_UNTRUSTED: i64 = 1;
const TRUST_SYSTEM: i64 = 2;

mod fuse;
mod query;

pub use fuse::{RecallComponents, RecallHit, RecallReport};
pub use query::RecallQuery;

/// FTS5 MATCH expression for the free-text query: bare terms ANDed, so every
/// word must appear somewhere in the document. The vec leg catches
/// paraphrases, so AND-precision is the right default for memory lookup.
fn fts_match_expr(text: &str) -> String {
    text.split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Vec-leg parameter blob (little-endian f32, same encoding as the write path).
fn blob(vec: &[f32]) -> Vec<u8> {
    vec.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Comma-quoted SQL list, e.g. `('working','semantic')`.
fn sql_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// A vec-leg hit before id resolution: (rowid, cosine distance).
type VecLegHit = (i64, f64);

/// An FTS-leg hit before fusion: (public_id, tier, trust).
type FtsLegHit = (String, String, String);

/// Run the two retrieval legs on one pooled read connection.
///
/// Returns vec hits as raw rowids (resolved to canonical rows afterwards) and
/// FTS hits with their canonical metadata, both already predicate-filtered.
async fn run_legs(
    store: &StoreHandle,
    q: &RecallQuery,
    query_vec: Option<Vec<f32>>,
) -> Result<(Vec<VecLegHit>, Vec<FtsLegHit>)> {
    let agent_id = store.agent_id().to_string();
    let text = q.text.clone();
    let tiers = q.tiers();
    let statuses = q.statuses();
    let trusts = q.trusts();
    let now = SystemClock.now_millis();
    let k = q.k;

    store
        .read(move |conn| {
            // ---- Vector leg: hard filters INSIDE the KNN scan (0031 step 1).
            // Untrusted/dead/other-tier vectors never occupy top-k slots.
            let mut vec_hits: Vec<VecLegHit> = Vec::new();
            if let Some(vec) = query_vec {
                let trust_codes = trusts
                    .iter()
                    .map(|t| match *t {
                        "trusted" => TRUST_TRUSTED,
                        "untrusted" => TRUST_UNTRUSTED,
                        "system" => TRUST_SYSTEM,
                        other => unreachable!("unknown trust {other} in allowlist"),
                    })
                    .map(|c| c.to_string())
                    .collect::<Vec<String>>()
                    .join(",");
                let status_codes = statuses
                    .iter()
                    .map(|s| match *s {
                        "active" => STATUS_ACTIVE,
                        "pending" => STATUS_PENDING,
                        other => unreachable!("unknown status {other} in allowlist"),
                    })
                    .map(|c| c.to_string())
                    .collect::<Vec<String>>()
                    .join(",");
                let tier_list = sql_list(tiers);
                let sql = format!(
                    "SELECT rowid, distance FROM vec_memories
                     WHERE embedding MATCH ?1 AND k = {k}
                       AND tier IN ({tier_list})
                       AND status IN ({status_codes})
                       AND trust IN ({trust_codes})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params![blob(&vec)], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                })?;
                for row in rows {
                    vec_hits.push(row?);
                }
            }

            // ---- Keyword leg: FTS5 joined to memories for the predicate
            // (0031 step 2 — fts_memories has no trust/status columns).
            let tier_list = sql_list(tiers);
            let status_list = sql_list(statuses);
            let trust_list = sql_list(trusts);
            let fts_sql = format!(
                "SELECT m.public_id, m.tier, m.trust FROM fts_memories
                 JOIN memories m ON m.id = fts_memories.rowid
                 WHERE fts_memories MATCH ?1
                   AND m.agent_id = ?2
                   AND m.tier IN ({tier_list})
                   AND m.status IN ({status_list})
                   AND m.trust IN ({trust_list})
                   AND (m.expires_at IS NULL OR m.expires_at > ?3)
                 LIMIT {k}"
            );
            let mut stmt = conn.prepare(&fts_sql)?;
            let expr = fts_match_expr(&text);
            let rows = stmt.query_map(rusqlite::params![expr, agent_id, now], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            let mut fts_hits: Vec<FtsLegHit> = Vec::new();
            for row in rows {
                fts_hits.push(row?);
            }
            Ok((vec_hits, fts_hits))
        })
        .await
}

/// Resolve vec rowids to canonical rows in one small lookup.
///
/// Returns `rowid -> (public_id, tier, status, trust)`. Orphaned rowids
/// (a vector without a canonical row) are simply absent from the map and get
/// dropped during fusion — they must never surface.
async fn resolve_vec_rows(
    store: &StoreHandle,
    rowids: &[i64],
) -> Result<std::collections::HashMap<i64, (String, String, String, String)>> {
    if rowids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let list = rowids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    store
        .read(move |conn| {
            let sql = format!(
                "SELECT id, public_id, tier, status, trust FROM memories
                 WHERE id IN ({list})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            let mut map = std::collections::HashMap::new();
            for row in rows {
                let (id, public_id, tier, status, trust) = row?;
                map.insert(id, (public_id, tier, status, trust));
            }
            Ok(map)
        })
        .await
}

/// Run one hybrid recall: embed → vec KNN + FTS5 → RRF fuse → re-filter.
pub async fn recall(
    store: &StoreHandle,
    embedder: &dyn Embedder,
    q: &RecallQuery,
) -> Result<RecallReport> {
    let started = std::time::Instant::now();

    if q.text.trim().is_empty() {
        return Err(Error::InvalidInput("recall query text is empty".into()));
    }

    // Embed the query once. Failure degrades to keyword-only (D4), never a
    // hard failure.
    let query_vec: Option<Vec<f32>> = match embedder.embed(std::slice::from_ref(&q.text)).await {
        Ok(mut v) if !v.is_empty() => Some(std::mem::take(&mut v[0])),
        _ => None,
    };
    let degraded = query_vec.is_none();

    let (vec_hits, fts_hits) = run_legs(store, q, query_vec).await?;

    let rowids: Vec<i64> = vec_hits.iter().map(|(id, _)| *id).collect();
    let rows = resolve_vec_rows(store, &rowids).await?;

    let hits = fuse::fuse(q, &vec_hits, &rows, &fts_hits);

    Ok(RecallReport {
        hits,
        degraded,
        latency_ms: started.elapsed().as_millis() as u64,
    })
}
