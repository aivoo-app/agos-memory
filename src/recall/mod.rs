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
//! scores never need calibrating. Hard-filter membership is defined once, in
//! [`filter`] (0032); both legs inline its predicate, and the fused id set is
//! re-gated against it afterwards (structural zero-leak guarantee).
//!
//! Rerank (0033) then rescores the fused pool with the D23 blend of normalized
//! rank, importance and per-tier decay (D24), and the final `top_k` is cut from
//! *that* order — fusion alone never decides the cut.
//!
//! Degraded mode (D4): when the embedder refuses (no provider / ceiling), the
//! vector leg is skipped, BM25-only results are returned, and
//! [`RecallReport::degraded`] is set — never a hard failure.

use crate::embed::Embedder;
use crate::error::{Error, Result};
use crate::storage::StoreHandle;
use crate::util::SystemClock;
use crate::util::clock::Clock;

use rusqlite::OptionalExtension;

mod filter;
mod fuse;
mod query;
mod rerank;

use filter::{load_by_rowids, surviving_public_ids};
use fuse::{FtsLegHit, VecLegHit};
use rerank::RERANK_POOL_FACTOR;

pub use filter::{CanonicalRow, HardFilter};
pub use fuse::{RecallComponents, RecallHit, RecallReport};
pub use query::RecallQuery;
pub use rerank::{decay, half_life_minutes};

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

/// Decode a cached LE-f32 blob, rejecting dimension drift.
fn decode_blob(bytes: &[u8], expected_dim: usize) -> Option<Vec<f32>> {
    if bytes.len() != expected_dim * 4 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect(),
    )
}

/// Query embedding with the `embeddings_cache` (0031 note: queries and writes
/// must stop double-spending embed calls). Cache reads and writes are
/// best-effort — any cache failure falls through to a plain embed, and an
/// embedder refusal still yields `None` (degraded mode).
async fn query_embedding(
    store: &StoreHandle,
    embedder: &dyn Embedder,
    text: &str,
) -> Result<Option<Vec<f32>>> {
    let model = embedder.model().to_string();
    let dim = embedder.dim();
    let hash = crate::util::sha256_hex(text);

    // Best-effort cache read.
    let cached: Option<Vec<f32>> = store
        .read({
            let hash = hash.clone();
            let model = model.clone();
            move |conn| {
                let blob: Option<Vec<u8>> = conn
                    .query_row(
                        "SELECT vector FROM embeddings_cache
                         WHERE content_hash = ?1 AND embed_model = ?2",
                        rusqlite::params![hash, model],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| Error::Storage(e.to_string()))?;
                Ok(blob.and_then(|b| decode_blob(&b, dim)))
            }
        })
        .await
        .unwrap_or(None);
    if let Some(vec) = cached {
        return Ok(Some(vec));
    }

    // Cache miss (or unreadable cache): embed.
    let text = text.to_string();
    let Some(vec) = embedder
        .embed(std::slice::from_ref(&text))
        .await
        .ok()
        .and_then(|mut v| {
            if v.is_empty() {
                None
            } else {
                Some(std::mem::take(&mut v[0]))
            }
        })
    else {
        return Ok(None); // degraded: no vec leg
    };

    // Best-effort cache write (single-writer thread); failures never fail
    // the recall.
    let bytes = blob(&vec);
    let now = SystemClock.now_millis();
    let _ = store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO embeddings_cache
                     (content_hash, embed_model, dim, vector, created_at, last_used_at, use_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1)
                 ON CONFLICT(content_hash, embed_model)
                 DO UPDATE SET last_used_at = ?5, use_count = use_count + 1",
                rusqlite::params![hash, model, dim as i64, bytes, now],
            )?;
            Ok(())
        })
        .await;

    Ok(Some(vec))
}

/// Run both legs on one pooled read connection, each leg gated by the *same*
/// hard filter (D6/D29 — filters run inside the legs, before scoring).
///
/// Both legs retrieve `pool` candidates and return rowids; the canonical rows
/// behind those rowids are loaded (and re-gated) by the caller, so the vec leg's
/// raw rowids and the FTS leg's join result end up verified the same way.
async fn run_legs(
    store: &StoreHandle,
    filter: &HardFilter,
    text: String,
    pool: usize,
    query_vec: Option<Vec<f32>>,
) -> Result<(Vec<VecLegHit>, Vec<FtsLegHit>)> {
    let vec_predicate = filter.vec_metadata_predicate();
    let predicate = filter.predicate("m");

    store
        .read(move |conn| {
            // ---- Vector leg: the metadata subset of the predicate is pushed
            // INSIDE the KNN scan, so disqualified vectors never occupy pool
            // slots (agent/expiry/supersede are enforced after the scan).
            let mut vec_hits: Vec<VecLegHit> = Vec::new();
            if let Some(vec) = query_vec {
                let sql = format!(
                    "SELECT rowid, distance FROM vec_memories
                     WHERE embedding MATCH ?1 AND k = {pool}
                       AND {vec_predicate}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params![blob(&vec)], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                })?;
                for row in rows {
                    vec_hits.push(row?);
                }
            }

            // ---- Keyword leg: FTS5 carries no trust/status columns, so it is
            // joined to `memories` and gated by the canonical predicate.
            let fts_sql = format!(
                "SELECT m.id, m.public_id, m.tier, m.trust FROM fts_memories
                 JOIN memories m ON m.id = fts_memories.rowid
                 WHERE fts_memories MATCH ?1 AND {predicate}
                 LIMIT {pool}"
            );
            let mut stmt = conn.prepare(&fts_sql)?;
            let expr = fts_match_expr(&text);
            let rows = stmt.query_map(rusqlite::params![expr], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
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

/// Run one hybrid recall: filter → embed → vec KNN + FTS5 → RRF fuse → re-gate.
pub async fn recall(
    store: &StoreHandle,
    embedder: &dyn Embedder,
    q: &RecallQuery,
) -> Result<RecallReport> {
    let started = std::time::Instant::now();

    if q.text.trim().is_empty() {
        return Err(Error::InvalidInput("recall query text is empty".into()));
    }

    // One filter for the whole call: same reference instant, same allowlists,
    // in both legs and in every gate that follows (0032).
    let filter = HardFilter::from_query(store.agent_id(), q, SystemClock.now_millis());

    // Embed the query (cached in embeddings_cache; 0031 note). Failure
    // degrades to keyword-only (D4), never a hard failure.
    let query_vec = query_embedding(store, embedder, &q.text).await?;
    let degraded = query_vec.is_none();

    // Each leg retrieves `top_k × RERANK_POOL_FACTOR` candidates: rerank
    // reorders by a different score than RRF, so a candidate the legs rank
    // 20th can still earn a top slot — it would be unreachable if the legs
    // stopped at `top_k`. The cut to `top_k` happens after rerank (0033).
    let pool = q.k.max(1).saturating_mul(RERANK_POOL_FACTOR);
    let (vec_hits, fts_hits) = run_legs(store, &filter, q.text.clone(), pool, query_vec).await?;

    // Gate 2: resolve *both* legs' rowids to canonical rows the filter admits.
    // Union, not just the vec leg: an FTS-only hit has no vec rowid, and
    // fusion drops any candidate it cannot verify against a canonical row.
    let mut rowids: Vec<i64> = vec_hits.iter().map(|(id, _)| *id).collect();
    rowids.extend(fts_hits.iter().map(|(id, _, _, _)| *id));
    rowids.sort_unstable();
    rowids.dedup();
    let rows = load_by_rowids(store, &filter, &rowids).await?;

    let fused = fuse::fuse(&filter, &vec_hits, &rows, &fts_hits);

    // Gate 3: re-apply the filter to the fused id set (structural zero-leak
    // guarantee — nothing unadmitted can survive fusion, whatever the legs did).
    let fused_ids: Vec<String> = fused.iter().map(|h| h.public_id.clone()).collect();
    let surviving = surviving_public_ids(store, &filter, &fused_ids).await?;
    let admitted: Vec<RecallHit> = fused
        .into_iter()
        .filter(|h| surviving.contains(&h.public_id))
        .collect();

    // Rerank (D23/D24), then cut: the `top_k` and `min_score` cuts must apply
    // to the *reranked* order, and the score cut may legitimately return fewer
    // than `k` hits (D26 — "report no hit" instead of a weak hit).
    let mut hits = rerank::rerank(&q.weights, &q.half_life, filter.now(), admitted, &rows);
    hits.truncate(q.k);
    hits.retain(|h| h.score >= q.min_score);

    Ok(RecallReport {
        hits,
        degraded,
        latency_ms: started.elapsed().as_millis() as u64,
    })
}
