//! Persist + dedup (issue 0028).
use rusqlite::OptionalExtension;

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
        DEDUP_THRESHOLD,
    )
    .await
}

/// Full variant with provenance + thresholds (issue 0029).
///
/// `E: ?Sized` so callers holding an owned `Box<dyn Embedder>` (CLI, eval
/// runner) can pass `&*embedder` without cloning the embedder.
#[allow(clippy::too_many_arguments)]
pub async fn persist_candidate_full<E: Embedder + ?Sized>(
    store: &StoreHandle,
    cand: &Candidate,
    embedder: &E,
    extractor_version: &str,
    source_kind: &str,
    pending_threshold: f64,
    dedup_threshold: f64,
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
    // Multi-agent isolation (D11/R8): every memory is stamped with the store's
    // agent. Omitting this column silently fell back to the schema default
    // `'default'`, which made writes invisible to recall for any other agent
    // (found by the end-to-end eval harness, issue 0052).
    let agent_owned = store.agent_id().to_string();
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
                // sqlite-vec `distance_metric=cosine` returns `1 - cos` directly
                // (see distance_cosine_float / cosine_float_neon in
                // crates/sqlite-vec-src/src/sqlite-vec.c), so similarity is the
                // plain complement — it is NOT a squared-L2 distance.
                let sim = 1.0 - dist.clamp(0.0, 2.0);
                if sim > dedup_threshold {
                    // D20: bump refcount + last-referenced, link `refines`, and
                    // tag the whole cluster so maintenance can collapse it.
                    let cluster: i64 = conn
                        .query_row(
                            "SELECT COALESCE(dedup_cluster_id, id) FROM memories WHERE id = ?1",
                            [rowid],
                            |r| r.get(0),
                        )
                        .optional()?
                        .unwrap_or(rowid);
                    conn.execute(
                        "UPDATE memories SET ref_count = ref_count + 1,
                         last_referenced_at = ?1, updated_at = ?1,
                         dedup_cluster_id = COALESCE(dedup_cluster_id, ?2)
                         WHERE id = ?3",
                        rusqlite::params![now, cluster, rowid],
                    )?;
                    conn.execute(
                        "INSERT OR IGNORE INTO memory_links
                         (from_memory_id, to_memory_id, kind, created_at)
                         VALUES (?1, ?1, 'refines', ?2)",
                        rusqlite::params![rowid, now],
                    )?;
                    let row = conn.query_row(
                        "SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at, summary_text, summary_tokens
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
                                updated_at: r.get(8)?,
                                summary_text: r.get(9)?,
                                summary_tokens: r.get(10).unwrap_or(0),
                            })
                        },
                    )?;
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
            // Integer codes mirror the memories-table CHECK constraints so the
            // vec0 row stays consistent with the canonical `memories` row.
            let status_code = match status {
                "active" => 0,
                "pending" => 1,
                "deprecated" => 2,
                "deleted" => 3,
                _ => unreachable!(),
            };
            let trust_code = match trust {
                "trusted" => 0,
                "untrusted" => 1,
                "system" => 2,
                _ => unreachable!(),
            };
            conn.execute(
                "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                 status, trust, source_kind, extractor_version, embed_model,
                 embed_dim, embed_status, ref_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                 ?10, ?11, ?12, 'ok', 0, ?13, ?13)",
                rusqlite::params![
                    &pid2,
                    &agent_owned,
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
                "INSERT INTO memory_versions (memory_id, version, text, text_hash, source_turn_id,
                        supersedes_version, change_reason, diff_json, created_by, created_at)
                 VALUES (?1, 1, ?2, ?3, NULL, NULL, NULL, NULL, NULL, ?4)",
                rusqlite::params![id, &cand.text, &hash, now],
            )?;
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)",
                rusqlite::params![id, &bytes, &cand.tier, status_code, trust_code, &cand.kind],
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
                    updated_at: now,
                    summary_text: None,
                    summary_tokens: 0,
                },
                deduped: false,
            })
        })
        .await?;
    Ok(report)
}

/// Degraded persist: no embedding provider (decision D4, `provider = none`).
///
/// Stores the redacted memory with `embed_status='skipped'` and no `vec_memories`
/// row, so keyword-only (FTS5) recall still finds it. Dedup falls back to
/// `text_hash` because there are no vectors to compare.
#[allow(clippy::too_many_arguments)]
pub async fn persist_candidate_degraded(
    store: &StoreHandle,
    cand: &Candidate,
    extractor_version: &str,
    source_kind: &str,
    pending_threshold: f64,
) -> Result<PersistReport> {
    persist_candidate_no_vector(
        store,
        cand,
        extractor_version,
        source_kind,
        pending_threshold,
        "skipped",
    )
    .await
}

/// Persist without a vector because the embedder *failed* (issue 0030).
///
/// Same shape as [`persist_candidate_degraded`] but records
/// `embed_status='failed'` instead of `'skipped'`, so a later `reembed` job can
/// tell "provider intentionally absent" from "provider was down and this row
/// still needs a vector". The fact itself is never lost to a provider outage.
#[allow(clippy::too_many_arguments)]
pub async fn persist_candidate_unembedded(
    store: &StoreHandle,
    cand: &Candidate,
    extractor_version: &str,
    source_kind: &str,
    pending_threshold: f64,
) -> Result<PersistReport> {
    persist_candidate_no_vector(
        store,
        cand,
        extractor_version,
        source_kind,
        pending_threshold,
        "failed",
    )
    .await
}

/// Shared no-vector persist. `embed_status` is `'skipped'` (provider = none) or
/// `'failed'` (provider errored); the schema restricts it to that pair plus
/// `'pending'`/`'ok'`.
#[allow(clippy::too_many_arguments)]
async fn persist_candidate_no_vector(
    store: &StoreHandle,
    cand: &Candidate,
    extractor_version: &str,
    source_kind: &str,
    pending_threshold: f64,
    embed_status: &'static str,
) -> Result<PersistReport> {
    let mut cand = cand.clone();
    cand.text = crate::memory::redact::redact(&cand.text);
    let ver = extractor_version.to_string();
    let source_owned = source_kind.to_string();
    // Same agent stamping as the embedded path (see `persist_candidate_full`).
    let agent_owned = store.agent_id().to_string();
    let report = store
        .write(move |conn| {
            let now = Clock::now_millis(&SystemClock);
            let hash = crate::util::sha256_hex(&cand.text);

            // Degraded dedup: identical text bumps the existing row.
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM memories WHERE text_hash = ?1 AND status IN ('active','pending')",
                    [&hash],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                conn.execute(
                    "UPDATE memories SET ref_count = ref_count + 1,
                     last_referenced_at = ?1, updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![now, id],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO memory_links
                     (from_memory_id, to_memory_id, kind, created_at)
                     VALUES (?1, ?1, 'refines', ?2)",
                    rusqlite::params![id, now],
                )?;
                let row = conn.query_row(
                    "SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at, summary_text, summary_tokens
                     FROM memories WHERE id = ?1",
                    [id],
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
                            updated_at: r.get(8)?,
                            summary_text: r.get(9)?,
                            summary_tokens: r.get(10).unwrap_or(0),
                        })
                    },
                )?;
                return Ok(PersistReport { row, deduped: true });
            }

            let pid = uuid::Uuid::new_v4().to_string();
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
                "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                 status, trust, source_kind, extractor_version, embed_status,
                 ref_count, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?12)",
                rusqlite::params![
                    &pid,
                    &agent_owned,
                    &cand.tier,
                    &cand.kind,
                    &cand.text,
                    &hash,
                    status,
                    trust,
                    &source_owned,
                    &ver,
                    embed_status,
                    now
                ],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO memory_versions (memory_id, version, text, text_hash, source_turn_id,
                        supersedes_version, change_reason, diff_json, created_by, created_at)
                 VALUES (?1, 1, ?2, ?3, NULL, NULL, NULL, NULL, NULL, ?4)",
                rusqlite::params![id, &cand.text, &hash, now],
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
                    updated_at: now,
                    summary_text: None,
                    summary_tokens: 0,
                },
                deduped: false,
            })
        })
        .await?;
    Ok(report)
}
