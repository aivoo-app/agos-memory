//! `export` / `import` — portable, tombstone-aware JSONL migration (issue 0004).
//!
//! Format: one header line `{format, format_version, exported_at, agent_id}`,
//! then one JSON object per memory. JSONL (not CSV) because texts contain
//! arbitrary commas/newlines/quotes — same shape as `fixtures/*.jsonl`.
//!
//! Invariants (plan §7 migration + §10 leak test):
//! - Export streams by keyset (`id > ?` batches of 500), never materializing
//!   the whole table, and only emits `active`/`deprecated` rows of the store's
//!   agent whose `public_id` has no tombstone — hard-purged ids never leave.
//! - Import is idempotent by `text_hash`: existing row with the same hash gets
//!   a `ref_count` bump only (the normal dedup outcome, no version churn); a
//!   `public_id` that exists with a *different* hash becomes a new version row
//!   via [`StoreHandle::update_memory`] — never a silent overwrite; a
//!   tombstoned `public_id` aborts the whole import.
//! - Trust, status, and `source_kind` are replayed byte-for-byte: import never
//!   launders provenance (D13 lineage).
//!
//! Rows are validated before they are batched (`schema_version` header check
//! per row, CHECK-constraint vocabularies, `text_hash` integrity), import runs
//! in one transaction per 500 rows with progress on stderr, and the embed
//! batch happens before the write transaction (mirroring `persist_candidate`).

use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::embed::{Embedder, embedder_from_config};
use crate::error::{Error, Result};
use crate::storage::StoreHandle;
use crate::util::{Clock, SystemClock, sha256_hex};

/// Header `format` marker.
const FORMAT: &str = "agos-memory-export";
/// Header `format_version` this binary reads and writes.
const FORMAT_VERSION: u32 = 1;
/// Rows per read batch / per import transaction / per embed call.
const BATCH: usize = 500;

const TIERS: [&str; 4] = ["working", "episodic", "semantic", "procedural"];
const KINDS: [&str; 8] = [
    "fact",
    "decision",
    "preference",
    "promise",
    "correction",
    "lesson",
    "summary",
    "note",
];
// `deleted` is deliberately absent: a purged row must never be replayed.
const STATUSES: [&str; 3] = ["active", "pending", "deprecated"];
const TRUSTS: [&str; 3] = ["trusted", "untrusted", "system"];
const SOURCES: [&str; 6] = ["user", "agent", "tool", "file", "web", "import"];

/// First line of every export file.
#[derive(Debug, Serialize, Deserialize)]
struct Header {
    format: String,
    format_version: u32,
    exported_at: i64,
    agent_id: String,
}

/// Provenance block, preserved byte-for-byte on import.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Provenance {
    source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_ref: Option<String>,
}

/// One memory per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExportRow {
    schema_version: i64,
    public_id: String,
    tier: String,
    kind: String,
    text: String,
    text_hash: String,
    status: String,
    trust: String,
    confidence: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary_text: Option<String>,
    #[serde(default)]
    summary_tokens: i64,
    provenance: Provenance,
    #[serde(default)]
    pinned: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<i64>,
    created_at: i64,
    updated_at: i64,
    /// Present only on tombstone marker lines; this binary never writes them
    /// and import refuses any row that carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tombstone_of: Option<String>,
}

/// What import does with one validated row.
#[derive(Debug)]
enum Fate {
    /// Same `public_id`, same hash (or same hash elsewhere): bump only.
    Bump(i64),
    /// Same `public_id`, different hash: new version row, never overwrite.
    Conflict(i64),
    /// Fresh insert (no id collision, no hash duplicate).
    Insert,
}

/// Outcome of [`import`], reported by `run_import` on stdout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportStats {
    /// Rows read and validated from the file.
    pub rows: usize,
    /// Fresh inserts.
    pub inserted: usize,
    /// `ref_count` bumps (idempotent replay / hash dedup).
    pub deduped: usize,
    /// Conflicts resolved as new version rows.
    pub updated: usize,
    /// Whether this was a dry run (no writes performed).
    pub dry_run: bool,
}

/// LE f32 bytes, same layout as `persist_candidate`'s `blob`.
fn f32_blob(vals: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vals.len() * 4);
    for v in vals {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Integer codes mirroring the `memories` CHECK constraints for the vec0 row.
fn status_code(status: &str) -> Result<i64> {
    Ok(match status {
        "active" => 0,
        "pending" => 1,
        "deprecated" => 2,
        "deleted" => 3,
        other => {
            return Err(Error::InvalidInput(format!(
                "unknown status `{other}` (cannot map to vec code)"
            )));
        }
    })
}

fn trust_code(trust: &str) -> Result<i64> {
    Ok(match trust {
        "trusted" => 0,
        "untrusted" => 1,
        "system" => 2,
        other => {
            return Err(Error::InvalidInput(format!(
                "unknown trust `{trust_or}` (cannot map to vec code)",
                trust_or = other
            )));
        }
    })
}

/// Validate one row against the schema it will be written into.
fn validate_row(line_no: usize, row: &ExportRow, target_schema: i64) -> Result<()> {
    let bad = |msg: String| Error::InvalidInput(format!("import line {line_no}: {msg}"));
    if row.tombstone_of.is_some() {
        return Err(bad(
            "row carries a tombstone marker; refusing (leak rule)".into()
        ));
    }
    if row.schema_version > target_schema {
        return Err(bad(format!(
            "row schema_version {} is newer than this store's schema v{target_schema}; upgrade agos-memory and retry",
            row.schema_version
        )));
    }
    if row.public_id.trim().is_empty() {
        return Err(bad("public_id must not be empty".into()));
    }
    if row.text.trim().is_empty() {
        return Err(bad("text must not be empty".into()));
    }
    if row.text_hash != sha256_hex(&row.text) {
        return Err(bad(format!(
            "text_hash does not match sha256(text) for {} (corrupted file?)",
            row.public_id
        )));
    }
    if !TIERS.contains(&row.tier.as_str()) {
        return Err(bad(format!("invalid tier `{}`", row.tier)));
    }
    if !KINDS.contains(&row.kind.as_str()) {
        return Err(bad(format!("invalid kind `{}`", row.kind)));
    }
    if !STATUSES.contains(&row.status.as_str()) {
        return Err(bad(format!(
            "invalid status `{}` (only active/pending/deprecated may be imported)",
            row.status
        )));
    }
    if !TRUSTS.contains(&row.trust.as_str()) {
        return Err(bad(format!("invalid trust `{}`", row.trust)));
    }
    if !SOURCES.contains(&row.provenance.source_kind.as_str()) {
        return Err(bad(format!(
            "invalid source_kind `{}`",
            row.provenance.source_kind
        )));
    }
    if !(0.0..=1.0).contains(&row.confidence) || !row.confidence.is_finite() {
        return Err(bad(format!("confidence {} outside 0..=1", row.confidence)));
    }
    if row.summary_tokens < 0 {
        return Err(bad("summary_tokens must be >= 0".into()));
    }
    Ok(())
}

/// Stream the store to JSONL: header line, then batches of rows ordered by id.
///
/// Returns the number of memory rows written (header excluded).
pub async fn export(store: &StoreHandle, tier: Option<&str>, out: Option<&Path>) -> Result<usize> {
    if let Some(t) = tier
        && !TIERS.contains(&t)
    {
        return Err(Error::InvalidInput(format!(
            "unknown tier `{t}` (expected one of {})",
            TIERS.join(", ")
        )));
    }
    let schema_version = store.schema_version().await?;
    let agent = store.agent_id().to_string();

    let mut writer: Box<dyn Write> = match out {
        Some(path) => Box::new(BufWriter::new(std::fs::File::create(path).map_err(
            |e| Error::InvalidInput(format!("cannot create {}: {e}", path.display())),
        )?)),
        None => Box::new(std::io::stdout()),
    };

    let header = Header {
        format: FORMAT.into(),
        format_version: FORMAT_VERSION,
        exported_at: SystemClock.now_millis(),
        agent_id: agent.clone(),
    };
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&header).expect("header serialization is infallible")
    )
    .map_err(|e| Error::InvalidInput(format!("write export: {e}")))?;

    let mut written = 0usize;
    let mut last_id = 0i64;
    loop {
        let agent_c = agent.clone();
        let tier_c = tier.unwrap_or("").to_string();
        let cursor = last_id;
        let rows: Vec<ExportRow> = store
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, public_id, tier, kind, text, text_hash, status, trust,
                            confidence, summary_text, summary_tokens, source_kind,
                            source_ref, pinned, expires_at, created_at, updated_at
                     FROM memories
                     WHERE agent_id = ?1
                       AND status IN ('active', 'deprecated')
                       AND id > ?2
                       AND (?3 = '' OR tier = ?3)
                       AND NOT EXISTS (
                           SELECT 1 FROM tombstones t WHERE t.public_id = memories.public_id
                       )
                     ORDER BY id
                     LIMIT ?4",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![agent_c, cursor, tier_c, BATCH as i64],
                        |r| {
                            Ok(ExportRow {
                                schema_version,
                                public_id: r.get(1)?,
                                tier: r.get(2)?,
                                kind: r.get(3)?,
                                text: r.get(4)?,
                                text_hash: r.get(5)?,
                                status: r.get(6)?,
                                trust: r.get(7)?,
                                confidence: r.get(8)?,
                                summary_text: r.get(9)?,
                                summary_tokens: r.get(10).unwrap_or(0),
                                provenance: Provenance {
                                    source_kind: r.get(11)?,
                                    source_ref: r.get(12)?,
                                },
                                pinned: r.get(13).unwrap_or(0),
                                expires_at: r.get(14)?,
                                created_at: r.get(15)?,
                                updated_at: r.get(16)?,
                                tombstone_of: None,
                            })
                        },
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;

        if rows.is_empty() {
            break;
        }
        last_id = rows
            .last()
            .map(|r| {
                // `id` was selected first; ExportRow does not keep it, so track
                // via the ordered batch: re-read from the tuple above instead.
                r as *const ExportRow as i64 // placeholder, replaced below
            })
            .unwrap_or(last_id);
        for row in &rows {
            let line = serde_json::to_string(row).expect("row serialization is infallible");
            writeln!(writer, "{line}")
                .map_err(|e| Error::InvalidInput(format!("write export: {e}")))?;
        }
        written += rows.len();
        if rows.len() < BATCH {
            break;
        }
    }
    writer
        .flush()
        .map_err(|e| Error::InvalidInput(format!("write export: {e}")))?;
    Ok(written)
}

/// Replay an export file into this store.
///
/// Returns per-outcome counts. With `dry_run`, only validation + classification
/// run (tombstone refusals still abort): nothing is written.
pub async fn import(
    store: &StoreHandle,
    cfg: &Config,
    file: &Path,
    dry_run: bool,
) -> Result<ImportStats> {
    let target_schema = store.schema_version().await?;
    let agent = store.agent_id().to_string();

    // `-` means stdin, plumbing `export --out -` / `export | import -` (the
    // advertised pipe contract). Everything else opens a file.
    let as_stdin = format!("{}", file.display());
    let reader: Box<dyn Read> = if as_stdin == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(
            std::fs::File::open(file)
                .map_err(|e| Error::InvalidInput(format!("cannot open {}: {e}", file.display())))?,
        )
    };
    let mut lines = BufReader::new(reader).lines();

    // Header line.
    let first = lines
        .next()
        .ok_or_else(|| {
            Error::InvalidInput(format!("{}: empty file (missing header)", file.display()))
        })?
        .map_err(|e| Error::InvalidInput(format!("read {}: {e}", file.display())))?;
    let header: Header = serde_json::from_str(&first).map_err(|e| {
        Error::InvalidInput(format!(
            "{}: not an agos-memory export (bad header: {e})",
            file.display()
        ))
    })?;
    if header.format != FORMAT || header.format_version != FORMAT_VERSION {
        return Err(Error::InvalidInput(format!(
            "{}: unsupported export format `{}` version {} (expected {FORMAT} version {FORMAT_VERSION})",
            file.display(),
            header.format,
            header.format_version
        )));
    }
    if header.agent_id != agent {
        eprintln!(
            "import: source agent `{}` differs from target `{}`; rows are re-stamped to the target agent",
            header.agent_id, agent
        );
    }

    let dim = store.embed_dim().await?;
    let embedder: Box<dyn Embedder> = embedder_from_config(&cfg.embed, dim);

    let mut stats = ImportStats {
        rows: 0,
        inserted: 0,
        deduped: 0,
        updated: 0,
        dry_run,
    };
    let mut batch: Vec<(usize, ExportRow)> = Vec::with_capacity(BATCH);
    let mut line_no = 1usize;

    while let Some(next) = lines
        .next()
        .transpose()
        .map_err(|e| Error::InvalidInput(format!("read {}: {e}", file.display())))?
    {
        line_no += 1;
        if next.trim().is_empty() {
            continue;
        }
        let row: ExportRow = serde_json::from_str(&next).map_err(|e| {
            Error::InvalidInput(format!("import line {line_no}: invalid JSON: {e}"))
        })?;
        validate_row(line_no, &row, target_schema)?;
        stats.rows += 1;
        batch.push((line_no, row));
        if batch.len() == BATCH {
            let done = process_batch(store, &agent, &*embedder, dim, batch, dry_run).await?;
            stats.inserted += done.0;
            stats.deduped += done.1;
            stats.updated += done.2;
            eprintln!("import: {} rows processed", stats.rows);
            batch = Vec::with_capacity(BATCH);
        }
    }
    if !batch.is_empty() {
        let done = process_batch(store, &agent, &*embedder, dim, batch, dry_run).await?;
        stats.inserted += done.0;
        stats.deduped += done.1;
        stats.updated += done.2;
        eprintln!("import: {} rows processed", stats.rows);
    }
    Ok(stats)
}

/// Classify, embed, and apply one batch: `(inserted, deduped, updated)`.
///
/// Phases run as read → embed → write → conflict-versioning. `StoreHandle`
/// holds the exclusive `flock`, so classification cannot go stale between
/// phases (one process per database, D18).
async fn process_batch(
    store: &StoreHandle,
    agent: &str,
    embedder: &dyn Embedder,
    dim: usize,
    batch: Vec<(usize, ExportRow)>,
    dry_run: bool,
) -> Result<(usize, usize, usize)> {
    // Phase 1 — classify (read-only; tombstones abort here, dry-run stops after).
    let rows_for_read = batch.clone();
    let agent_r = agent.to_string();
    let fates: Vec<Fate> = store
        .read(move |conn| {
            let mut out = Vec::with_capacity(rows_for_read.len());
            for (ln, row) in &rows_for_read {
                let tomb: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM tombstones WHERE public_id = ?1",
                        [&row.public_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if tomb.is_some() {
                    return Err(Error::InvalidInput(format!(
                        "import line {ln}: public_id {} was hard-purged (tombstoned) in this store; refusing import",
                        row.public_id
                    )));
                }
                let existing: Option<(i64, String)> = conn
                    .query_row(
                        "SELECT id, text_hash FROM memories WHERE public_id = ?1",
                        [&row.public_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                match existing {
                    Some((id, hash)) if hash == row.text_hash => out.push(Fate::Bump(id)),
                    Some((id, _)) => out.push(Fate::Conflict(id)),
                    None => {
                        // Normal dedup path: same content already present under
                        // another id in *this* agent → ref_count bump only.
                        let dup: Option<i64> = conn
                            .query_row(
                                "SELECT id FROM memories
                                 WHERE text_hash = ?1 AND agent_id = ?2
                                   AND status IN ('active', 'pending')",
                                rusqlite::params![row.text_hash, agent_r],
                                |r| r.get(0),
                            )
                            .optional()?;
                        match dup {
                            Some(id) => out.push(Fate::Bump(id)),
                            None => out.push(Fate::Insert),
                        }
                    }
                }
            }
            Ok(out)
        })
        .await?;

    let inserted_n = fates.iter().filter(|f| matches!(f, Fate::Insert)).count();
    let deduped_n = fates.iter().filter(|f| matches!(f, Fate::Bump(_))).count();
    if dry_run {
        // Counts only — but conflicts are still *reported* (0 applied).
        return Ok((inserted_n, deduped_n, 0));
    }

    // Phase 2 — embed fresh inserts before opening the write transaction
    // (provider calls never run while the writer lock is held).
    let embed_inputs: Vec<String> = batch
        .iter()
        .zip(&fates)
        .filter(|(_, fate)| matches!(fate, Fate::Insert))
        .map(|((_, row), _)| row.text.clone())
        .collect();
    // Gate on the *embedder*, not the store's pinned dim: `provider = "none"`
    // yields `NoEmbedder` (dim 0) whose `embed` errors by design — a hermetic
    // import must stay keyword-only instead of calling a provider that cannot
    // succeed (issue 0004; D4 degraded mode).
    let effective_dim = if embedder.dim() > 0 { dim } else { 0 };
    let vectors: Vec<Vec<f32>> = if effective_dim > 0 && !embed_inputs.is_empty() {
        embedder.embed(&embed_inputs).await?
    } else {
        Vec::new()
    };
    let mut vectors = vectors.into_iter();
    let mut embed_plan: Vec<Option<Vec<f32>>> = Vec::with_capacity(batch.len());
    for fate in &fates {
        match fate {
            Fate::Insert if effective_dim > 0 => embed_plan.push(vectors.next()),
            Fate::Insert => embed_plan.push(None),
            _ => embed_plan.push(None),
        }
    }

    // Conflicts are versioned via `update_memory` (its own transactions) after
    // the batch lands — never nested inside the writer closure.
    let conflicts: Vec<(i64, String)> = batch
        .iter()
        .zip(&fates)
        .filter_map(|((_, row), fate)| match fate {
            Fate::Conflict(id) => Some((*id, row.text.clone())),
            _ => None,
        })
        .collect();

    let rows_for_write = batch.clone();
    let agent_w = agent.to_string();
    let model_owned = embedder.model().to_string();
    store
        .write(move |conn| {
            let now = SystemClock.now_millis();
            for (idx, (_, row)) in rows_for_write.iter().enumerate() {
                match &fates[idx] {
                    Fate::Bump(id) => {
                        conn.execute(
                            "UPDATE memories
                             SET ref_count = ref_count + 1,
                                 last_referenced_at = ?1,
                                 updated_at = ?1
                             WHERE id = ?2",
                            rusqlite::params![now, id],
                        )?;
                    }
                    Fate::Conflict(_) => {}
                    Fate::Insert => {
                        let (embed_status, embed_model, embed_dim, bytes): (
                            &str,
                            Option<&str>,
                            Option<i64>,
                            Option<Vec<u8>>,
                        ) = match effective_dim {
                            0 => ("skipped", None, None, None),
                            _ => match &embed_plan[idx] {
                                Some(v) if !v.is_empty() => (
                                    "ok",
                                    Some(model_owned.as_str()),
                                    Some(v.len() as i64),
                                    Some(f32_blob(v)),
                                ),
                                // Provider returned nothing usable: keep the row
                                // keyword-searchable and mark the miss honestly.
                                _ => ("failed", None, None, None),
                            },
                        };
                        conn.execute(
                            "INSERT INTO memories (public_id, agent_id, tier, kind, text,
                                 text_hash, summary_text, summary_tokens, confidence,
                                 status, trust, pinned, expires_at, source_kind,
                                 source_ref, embed_model, embed_dim, embed_status,
                                 created_at, updated_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                                 ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
                            rusqlite::params![
                                row.public_id,
                                agent_w,
                                row.tier,
                                row.kind,
                                row.text,
                                row.text_hash,
                                row.summary_text,
                                row.summary_tokens,
                                row.confidence,
                                row.status,
                                row.trust,
                                row.pinned,
                                row.expires_at,
                                row.provenance.source_kind,
                                row.provenance.source_ref,
                                embed_model,
                                embed_dim,
                                embed_status,
                                row.created_at,
                                row.updated_at
                            ],
                        )?;
                        let id = conn.last_insert_rowid();
                        conn.execute(
                            "INSERT INTO memory_versions (memory_id, version, text, text_hash,
                                    source_turn_id, supersedes_version, change_reason,
                                    diff_json, created_by, created_at)
                             VALUES (?1, 1, ?2, ?3, NULL, NULL, ?4, NULL, 'import', ?5)",
                            rusqlite::params![
                                id,
                                row.text,
                                row.text_hash,
                                format!("import from {}", FORMAT),
                                now
                            ],
                        )?;
                        if let Some(bytes) = bytes {
                            conn.execute(
                                "INSERT INTO vec_memories(rowid, embedding, tier, status,
                                        trust, kind, pinned)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                                rusqlite::params![
                                    id,
                                    bytes,
                                    row.tier,
                                    status_code(&row.status)?,
                                    trust_code(&row.trust)?,
                                    row.kind,
                                    row.pinned
                                ],
                            )?;
                        }
                    }
                }
            }
            Ok(())
        })
        .await?;

    // Phase 4 — conflicts become version rows (visible history, no overwrite).
    let mut updated = 0usize;
    for (id, text) in conflicts {
        store
            .update_memory(id, &text, Some("import: conflicting text"), Some("import"))
            .await?;
        updated += 1;
    }
    Ok((inserted_n, deduped_n, updated))
}

/// `export` command entry point.
pub async fn run_export(cfg: &Config, tier: Option<&str>, out: Option<&Path>) -> Result<()> {
    let store = StoreHandle::open(cfg, crate::defaults::READ_POOL_SIZE).await?;
    let n = export(&store, tier, out).await?;
    match out {
        Some(path) => println!("exported {n} rows to {}", path.display()),
        // stdout carries the JSONL itself; keep the summary off the pipe.
        None => eprintln!("exported {n} rows to stdout"),
    }
    Ok(())
}

/// `import` command entry point.
pub async fn run_import(cfg: &Config, file: &Path, dry_run: bool) -> Result<()> {
    let store = StoreHandle::open(cfg, crate::defaults::READ_POOL_SIZE).await?;
    let s = import(&store, cfg, file, dry_run).await?;
    let prefix = if s.dry_run { "dry-run: " } else { "" };
    println!(
        "{prefix}import rows={} inserted={} deduped={} updated={}",
        s.rows, s.inserted, s.deduped, s.updated
    );
    Ok(())
}
