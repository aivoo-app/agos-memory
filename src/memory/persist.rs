//! Persist + dedup (issue 0028).
use crate::embed::Embedder;
use crate::error::Result;
use crate::memory::extract::Candidate;
use crate::storage::{MemoryRow, StoreHandle};
use crate::util::{Clock, SystemClock};
/// Cosine similarity above which a candidate is a duplicate.
pub const DEDUP_THRESHOLD: f64 = 0.92;
/// Outcome of [`persist_candidate`].
#[derive(Debug, Clone, PartialEq)]
pub struct PersistReport {
    /// The surviving row (existing on dedup, new on insert).
    pub row: MemoryRow,
    /// True when an existing memory was bumped instead of inserting.
    pub deduped: bool,
}
fn blob(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}
/// Embed + dedup + insert one candidate; returns the surviving row.
///
/// Everything runs inside one writer closure (one transaction): the KNN
/// lookup, the refcount bump or the fresh insert of `memories` +
/// `memory_versions` v1 + `vec_memories`.
///
/// Trust: `source_kind` web/tool/import forces `trust='untrusted'`.
/// Low confidence (< `pending_threshold`) forces `status='pending'`.
/// Text is redacted BEFORE embedding so secrets never reach the provider.
pub async fn persist_candidate<E: Embedder>(
    store: &StoreHandle,
    cand: &Candidate,
    embedder: &E,
    extractor_version: &str,
) -> Result<PersistReport> {
    persist_candidate_full(
        store,
        cand,
        embedder,
        extractor_version,
        "user",
        crate::memory::PENDING_THRESHOLD_DEFAULT,
    )
    .await
}

/// Full variant with provenance + pending threshold (issue 0029).
#[allow(clippy::too_many_arguments)]
pub async fn persist_candidate_full<E: Embedder>(
    store: &StoreHandle,
    cand: &Candidate,
    embedder: &E,
    extractor_version: &str,
    source_kind: &str,
    pending_threshold: f64,
) -> Result<PersistReport> {
    let mut cand = cand.clone();
    cand.text = crate::memory::redact::redact(&cand.text);
    let vecs = embedder.embed(std::slice::from_ref(&cand.text)).await?;
    let vec = vecs.into_iter().next().unwrap_or_default();
    let bytes = blob(&vec);
    let dim = vec.len() as i64;
    let model = embedder.model().to_string();
    let cand = cand.clone();
    let ver = extractor_version.to_string();
    let source_owned = source_kind.to_string();
    let report = store
        .write(move |conn| {
            let now = Clock::now_millis(&SystemClock);
            let mut stmt = conn.prepare(
                "SELECT rowid, distance FROM vec_memories
                 WHERE embedding MATCH ?1 AND k = 3 AND tier = ?2
                 ORDER BY distance",
            )?;
            let best: Option<(i64, f64)> = stmt
                .query_map(rusqlite::params![&bytes, &cand.tier], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                })?
                .filter_map(|r| r.ok())
                .next();
            if let Some((rowid, dist)) = best {
                // sqlite-vec cosine distance on unit vectors: d²/2 = 1 - cos.
                let sim = 1.0 - dist * dist / 2.0;
                if sim > DEDUP_THRESHOLD {
                    let pid: String = conn.query_row(
                        "SELECT public_id FROM memories WHERE id = ?1",
                        [rowid],
                        |r| r.get(0),
                    )?;
                    conn.execute(
                        "UPDATE memories SET ref_count = ref_count + 1,
                         last_referenced_at = ?1, updated_at = ?1 WHERE id = ?2",
                        rusqlite::params![now, rowid],
                    )?;
                    conn.execute(
                        "INSERT OR IGNORE INTO memory_links
                         (from_memory_id, to_memory_id, kind, created_at)
                         VALUES (?1, ?1, 'refines', ?2)",
                        rusqlite::params![rowid, now],
                    )?;
                    let row = conn.query_row(
                        "SELECT id, public_id, tier, kind, text, status, trust, created_at
                         FROM memories WHERE id = ?1",
                        [rowid],
                        |r| {
                            Ok(MemoryRow {
                                id: r.get(0)?,
                                public_id: r.get(1)?,
                                tier: r.get(2)?,
                                kind: r.get(3)?,
                                text: r.get(4)?,
                                status: r.get(5)?,
                                trust: r.get(6)?,
                                created_at: r.get(7)?,
                            })
                        },
                    )?;
                    let _ = pid;
                    return Ok(PersistReport { row, deduped: true });
                }
            }
            let pid = uuid::Uuid::new_v4().to_string();
            let hash = crate::util::sha256_hex(&cand.text);
            let pid2 = pid.clone();
            let trust = match source_owned.as_str() {
                "tool" | "web" | "import" => "untrusted",
                _ => "trusted",
            };
            let status = if cand.confidence < pending_threshold {
                "pending"
            } else {
                "active"
            };
            conn.execute(
                "INSERT INTO memories (public_id, tier, kind, text, text_hash,
                 status, trust, source_kind, extractor_version, embed_model,
                 embed_dim, embed_status, ref_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                 ?9, ?10, ?11, 'ok', 0, ?12, ?12)",
                rusqlite::params![
                    &pid2,
                    &cand.tier,
                    &cand.kind,
                    &cand.text,
                    &hash,
                    status,
                    trust,
                    &source_owned,
                    &ver,
                    &model,
                    dim,
                    now
                ],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO memory_versions (memory_id, version, text, text_hash, created_at)
                 VALUES (?1, 1, ?2, ?3, ?4)",
                rusqlite::params![id, &cand.text, &hash, now],
            )?;
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, 0, 0, ?4, 0)",
                rusqlite::params![id, &bytes, &cand.tier, &cand.kind],
            )?;
            Ok(PersistReport {
                row: MemoryRow {
                    id,
                    public_id: pid,
                    tier: cand.tier.clone(),
                    kind: cand.kind.clone(),
                    text: cand.text.clone(),
                    status: status.into(),
                    trust: trust.into(),
                    created_at: now,
                },
                deduped: false,
            })
        })
        .await?;
    Ok(report)
}
