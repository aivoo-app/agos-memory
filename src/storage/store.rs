//! Store facade (issue 0008): one writer + read pool + process lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::OptionalExtension;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::storage::pool::ReadPool;
use crate::storage::schema::{self, SCHEMA_VERSION};
use crate::storage::vecext;
use crate::storage::writer::WriterHandle;
use crate::util::Clock;
use serde_json;

/// A single version of a memory row.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryVersion {
    /// Internal version id.
    pub id: i64,
    /// Parent memory id.
    pub memory_id: i64,
    /// Version number (1, 2, 3, ...).
    pub version: i64,
    /// Text at this version.
    pub text: String,
    /// SHA-256 of text.
    pub text_hash: String,
    /// Source turn id (optional).
    pub source_turn_id: Option<i64>,
    /// Previous version number (optional).
    pub supersedes_version: Option<i64>,
    /// Why this version was created.
    pub change_reason: Option<String>,
    /// JSON diff from previous version.
    pub diff_json: Option<String>,
    /// Who/what created this version.
    pub created_by: Option<String>,
    /// Creation time (UTC epoch millis).
    pub created_at: i64,
}

/// A tombstone row (hard-purged memory record).
#[derive(Debug, Clone, PartialEq)]
pub struct TombstoneRow {
    pub id: i64,
    pub public_id: String,
    pub agent_id: String,
    pub memory_id: Option<i64>,
    pub text_hash: String,
    pub deleted_at: i64,
    pub deleted_by: Option<String>,
    pub reason: Option<String>,
    pub rowcount_before: i64,
    pub rowcount_after: i64,
    pub vacuum_duration_ms: i64,
}

/// A forget_audit row.
#[derive(Debug, Clone, PartialEq)]
pub struct ForgetAuditRow {
    pub id: i64,
    pub action: String,
    pub selector_json: String,
    pub memory_ids: String,
    pub requester: String,
    pub reason: Option<String>,
    pub created_at: i64,
}

/// A single memory row as read back from storage.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRow {
    /// Internal row id.
    pub id: i64,
    /// External, stable identifier.
    pub public_id: String,
    /// Memory tier: working / episodic / semantic / procedural.
    pub tier: String,
    /// Memory kind: fact / decision / preference / ...
    pub kind: String,
    /// The memory text.
    pub text: String,
    /// active / pending / deprecated / deleted.
    pub status: String,
    /// trusted / untrusted / system.
    pub trust: String,
    /// Creation time (UTC epoch millis).
    pub created_at: i64,
    /// Last update time (UTC epoch millis).
    pub updated_at: i64,
    /// Summary text (optional, for summary-swap).
    pub summary_text: Option<String>,
    /// Token count of summary.
    pub summary_tokens: i64,
}

/// Data for a new memory; storage assigns ids and timestamps.
#[derive(Debug, Clone)]
pub struct NewMemory {
    /// Memory tier.
    pub tier: String,
    /// Memory kind.
    pub kind: String,
    /// The memory text.
    pub text: String,
    /// Source of the memory (user / agent / tool / file / web / import).
    pub source_kind: String,
}

/// Derive trust from provenance. The source vocabulary is validated by the
/// SQLite schema; unknown values are treated conservatively as untrusted.
pub(crate) fn trust_for_source_kind(source_kind: &str) -> &'static str {
    match source_kind {
        "user" | "agent" => "trusted",
        _ => "untrusted",
    }
}

/// Merge provenance into an existing trust value without allowing an
/// untrusted source to be upgraded by a later trusted operation.
pub(crate) fn trust_after_source(current: &str, source_kind: &str) -> &'static str {
    if current == "untrusted" || trust_for_source_kind(source_kind) == "untrusted" {
        "untrusted"
    } else if current == "system" {
        "system"
    } else {
        "trusted"
    }
}

/// Result of a verified snapshot ([`StoreHandle::snapshot_to`]).
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotReport {
    /// Snapshot file path.
    pub path: PathBuf,
    /// Snapshot size in bytes.
    pub bytes: u64,
    /// `PRAGMA integrity_check` result of the snapshot (must be `"ok"`).
    pub integrity: String,
    /// Row counts per core table in the snapshot (verification detail).
    pub tables: Vec<(String, i64)>,
}

/// Compute a simple line-based diff between two texts.
fn compute_line_diff(old: &str, new: &str) -> LineDiff {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let mut added = Vec::new();
    let mut removed = Vec::new();

    // Simple LCS-based diff
    let lcs = compute_lcs(&old_lines, &new_lines);
    let mut i = 0;
    let mut j = 0;
    for &len in &lcs {
        // Lines in old before the LCS match are removed
        while i < len.0 {
            removed.push(old_lines[i].to_string());
            i += 1;
        }
        // Lines in new before the LCS match are added
        while j < len.1 {
            added.push(new_lines[j].to_string());
            j += 1;
        }
        // Skip the matched line
        i += 1;
        j += 1;
    }
    // Remaining lines
    while i < old_lines.len() {
        removed.push(old_lines[i].to_string());
        i += 1;
    }
    while j < new_lines.len() {
        added.push(new_lines[j].to_string());
        j += 1;
    }

    LineDiff { added, removed }
}

struct LineDiff {
    added: Vec<String>,
    removed: Vec<String>,
}

/// Compute the longest common subsequence of two slices of lines.
/// Returns a list of (old_index, new_index) pairs that are common.
fn compute_lcs(old: &[&str], new: &[&str]) -> Vec<(usize, usize)> {
    let m = old.len();
    let n = new.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in 1..=m {
        for j in 1..=n {
            if old[i - 1] == new[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = std::cmp::max(dp[i - 1][j], dp[i][j - 1]);
            }
        }
    }

    // Backtrack to find the LCS pairs
    let mut result = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if old[i - 1] == new[j - 1] {
            result.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] >= dp[i][j - 1] {
            i -= 1;
        } else {
            j -= 1;
        }
    }
    result.reverse();
    result
}

/// Row counts of the core tables, used to verify a snapshot against the live
/// database. Table names are a fixed list — no injection surface.
fn table_counts(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut out = Vec::new();
    for t in [
        "agents",
        "meta",
        "sessions",
        "turns",
        "memories",
        "memory_versions",
        "memory_links",
        "embeddings_cache",
        "jobs",
        "jobs_dead",
        "llm_calls",
        "token_ledger",
        "recalls",
        "recall_items",
        "pins",
        "forget_audit",
        "tombstones",
    ] {
        let n: i64 = conn.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0))?;
        out.push((t.to_string(), n));
    }
    Ok(out)
}

/// Handle to an open store: writer + read pool. Cloneable.
#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<Store>,
}

impl std::fmt::Debug for StoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreHandle")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

/// The owned store resources.
pub struct Store {
    /// Database file path.
    pub path: PathBuf,
    /// Agent ID owning this database.
    pub agent_id: String,
    /// The single writer.
    pub writer: WriterHandle,
    /// Round-robin read connections.
    pub reads: ReadPool,
    /// Process lock: keeps the DB serialized to one process until this Store
    /// is dropped.
    #[allow(dead_code)]
    lock: ProcessLock,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .field("read_pool_size", &self.reads.size())
            .finish_non_exhaustive()
    }
}

impl StoreHandle {
    /// Borrow the agent ID this store was opened for.
    pub fn agent_id(&self) -> &str {
        &self.inner.agent_id
    }

    /// Whether the single-writer actor is still healthy and responsive to
    /// scheduled work. This is read-only and does not submit a database job.
    pub fn writer_is_healthy(&self) -> bool {
        self.inner.writer.is_healthy()
    }

    /// Open (or create) the database at `cfg.db_path`, migrate, register
    /// sqlite-vec, create the vec0 table, and enforce the single-process rule.
    pub async fn open(cfg: &Config, read_pool_size: usize) -> Result<Self> {
        let path = cfg.db_path.clone();
        let agent_id = cfg.agent_id.clone();

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Storage(format!("cannot create {}: {e}", parent.display())))?;
        }

        let lock = ProcessLock::acquire(&path)?;

        // Register sqlite-vec once per process before any connection is used.
        vecext::register()?;

        // Open + migrate (blocking), then hand the connection to the writer.
        let conn = Connection::open(&path)
            .map_err(|e| Error::Storage(format!("cannot open {}: {e}", path.display())))?;
        schema::apply_pragmas(&conn)?;
        let mut conn = conn;
        schema::migrate(&mut conn)?;

        // Embedding dim: pinned in `meta` at first init, enforced at recall time.
        let dim: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'embed_dim'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let dim = if dim == 0 { 1536 } else { dim as u32 };
        schema::ensure_vec_table(&conn, dim)?;

        // Seed the agent row (single-tenant v0.1.0).
        conn.execute(
            "INSERT INTO agents(id, created_at) VALUES (?1, ?2)
             ON CONFLICT(id) DO NOTHING",
            rusqlite::params![agent_id, crate::util::SystemClock.now_millis()],
        )?;

        let reads = ReadPool::open(&path, read_pool_size)?;
        let writer = WriterHandle::spawn(conn)?;
        Ok(Self {
            inner: Arc::new(Store {
                path,
                agent_id,
                writer,
                reads,
                lock,
            }),
        })
    }

    /// Validate that the configured embedder's dimension matches the database.
    /// Should be called after creating an embedder, before any recall operations.
    pub async fn validate_embed_dim(&self, embedder: &dyn crate::embed::Embedder) -> Result<()> {
        let embedder_dim = embedder.dim();
        let embedder_model = embedder.model().to_string();

        self.read(move |conn| {
            let stored_dim: i64 = conn
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'embed_dim'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let stored_dim = if stored_dim == 0 {
                1536
            } else {
                stored_dim as usize
            };

            if stored_dim != embedder_dim {
                return Err(Error::EmbeddingMismatch {
                    model: embedder_model.clone(),
                    dim: stored_dim as i64,
                    found: embedder_model,
                    found_dim: embedder_dim as i64,
                });
            }
            Ok(())
        })
        .await
    }

    /// Embedding dimension pinned in `meta` at first init (1536 by default).
    ///
    /// Used by provider construction so the embedder matches the `vec0` table.
    pub async fn embed_dim(&self) -> Result<usize> {
        self.read(|conn| {
            let stored: i64 = conn
                .query_row(
                    "SELECT CAST(value AS INTEGER) FROM meta WHERE key = 'embed_dim'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            Ok(if stored == 0 { 1536 } else { stored as usize })
        })
        .await
    }

    /// Run a blocking read on a pooled connection, off the async runtime.
    pub async fn read<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        self.read_timeout(None, f).await
    }

    /// Run a blocking read on a pooled connection with a timeout.
    ///
    /// Returns `Error::Storage("read pool timeout")` if no connection is available
    /// within the timeout.
    pub async fn read_timeout<T, F>(&self, timeout: Option<Duration>, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = self
            .inner
            .reads
            .get_timeout(timeout)
            .ok_or_else(|| Error::Storage("read pool timeout".into()))?;
        tokio::task::spawn_blocking(move || f(&conn))
            .await
            .map_err(|e| Error::Storage(format!("read task panicked: {e}")))?
    }

    /// Run a blocking write on the writer thread, off the async runtime.
    pub async fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let writer = self.inner.writer.clone();
        tokio::task::spawn_blocking(move || writer.write(f))
            .await
            .map_err(|e| Error::Storage(format!("write task panicked: {e}")))?
    }

    /// Maximum allowed memory text size (1 MiB).
    const MAX_MEMORY_TEXT_SIZE: usize = 1_048_576;

    /// Insert a memory row; returns the inserted row.
    pub async fn insert_memory(&self, m: NewMemory) -> Result<MemoryRow> {
        // Validate input size to prevent DoS.
        if m.text.len() > Self::MAX_MEMORY_TEXT_SIZE {
            return Err(Error::InvalidInput(format!(
                "memory text exceeds maximum size of {} bytes",
                Self::MAX_MEMORY_TEXT_SIZE
            )));
        }
        if m.tier.len() > 64 || m.kind.len() > 64 || m.source_kind.len() > 64 {
            return Err(Error::InvalidInput(
                "tier, kind, or source_kind exceeds maximum length of 64 characters".into(),
            ));
        }

        let public_id = uuid::Uuid::new_v4().to_string();
        let text_hash = crate::util::sha256_hex(&m.text);
        let agent_id = self.inner.agent_id.clone();
        let tier = m.tier.clone();
        let kind = m.kind.clone();
        let text = m.text.clone();
        let source_kind = m.source_kind.clone();
        let trust = trust_for_source_kind(&source_kind);

        let now = crate::util::SystemClock.now_millis();
        let pid = public_id.clone();

        let row = self
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                                           status, trust, source_kind, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?8, ?9, ?10)",
                    rusqlite::params![
                        &pid,
                        agent_id,
                        tier,
                        kind,
                        text,
                        text_hash,
                        trust,
                        source_kind,
                        now,
                        now
                    ],
                )?;
                let id = conn.last_insert_rowid();
                // Insert initial version
                conn.execute(
                    "INSERT INTO memory_versions (memory_id, version, text, text_hash, source_turn_id,
                            supersedes_version, change_reason, diff_json, created_by, created_at)
                     VALUES (?1, 1, ?2, ?3, NULL, NULL, NULL, NULL, NULL, ?4)",
                    rusqlite::params![id, &m.text, &text_hash, now],
                )?;
                Ok(MemoryRow {
                    id,
                    public_id: pid,
                    tier: m.tier.clone(),
                    kind: m.kind.clone(),
                    text: m.text.clone(),
                    status: "active".into(),
                    trust: trust.into(),
                    created_at: now,
                    updated_at: now,
                    summary_text: None,
                    summary_tokens: 0,
                })
            })
            .await?;

        Ok(row)
    }

    /// Attach a source reference to an existing memory.
    ///
    /// Ingest adapters use this after the normal redacted/embedded write path
    /// returns. The reference is provenance metadata only; it never changes the
    /// text or trust value.
    pub async fn set_source_ref(&self, memory_id: i64, source_ref: &str) -> Result<()> {
        if source_ref.trim().is_empty() || source_ref.len() > 1024 {
            return Err(Error::InvalidInput(
                "source_ref must be 1..=1024 bytes".into(),
            ));
        }
        let source_ref = source_ref.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE memories SET source_ref = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![
                    &source_ref,
                    crate::util::SystemClock.now_millis(),
                    memory_id
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Count memories grouped by status.
    pub async fn memory_counts(&self) -> Result<Vec<(String, i64)>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT status, count(*) FROM memories GROUP BY status ORDER BY status")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Fetch one memory by public id.
    pub async fn get_memory(&self, public_id: String) -> Result<Option<MemoryRow>> {
        self.read(move |conn| {
            let q = "
                SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at, summary_text, summary_tokens
                FROM memories WHERE public_id = ?1";
            match conn.query_row(q, [public_id], |r| {
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
            }) {
                Ok(m) => Ok(Some(m)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Fetch one memory by internal id.
    pub async fn get_memory_by_id(&self, id: i64) -> Result<Option<MemoryRow>> {
        self.read(move |conn| {
            let q = "
                SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at, summary_text, summary_tokens
                FROM memories WHERE id = ?1";
            match conn.query_row(q, [id], |r| {
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
            }) {
                Ok(m) => Ok(Some(m)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Fetch all versions of a memory by its internal id, ordered by version ASC.
    pub async fn get_memory_versions(&self, memory_id: i64) -> Result<Vec<MemoryVersion>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, memory_id, version, text, text_hash, source_turn_id,
                        supersedes_version, change_reason, diff_json, created_by, created_at
                 FROM memory_versions WHERE memory_id = ?1
                 ORDER BY version ASC",
            )?;
            let iter = stmt.query_map([memory_id], |r| {
                Ok(MemoryVersion {
                    id: r.get(0)?,
                    memory_id: r.get(1)?,
                    version: r.get(2)?,
                    text: r.get(3)?,
                    text_hash: r.get(4)?,
                    source_turn_id: r.get(5)?,
                    supersedes_version: r.get(6)?,
                    change_reason: r.get(7)?,
                    diff_json: r.get(8)?,
                    created_by: r.get(9)?,
                    created_at: r.get(10)?,
                })
            })?;
            let mut versions = Vec::new();
            for row in iter {
                versions.push(row.map_err(|e| crate::error::Error::Storage(e.to_string()))?);
            }
            Ok(versions)
        })
        .await
    }

    /// Fetch a single version by (memory_id, version) pair.
    pub async fn get_memory_version(
        &self,
        memory_id: i64,
        version: i64,
    ) -> Result<Option<MemoryVersion>> {
        self.read(move |conn| {
            let q = "
                SELECT id, memory_id, version, text, text_hash, source_turn_id,
                        supersedes_version, change_reason, diff_json, created_by, created_at
                FROM memory_versions WHERE memory_id = ?1 AND version = ?2";
            match conn.query_row(q, [memory_id, version], |r| {
                Ok(MemoryVersion {
                    id: r.get(0)?,
                    memory_id: r.get(1)?,
                    version: r.get(2)?,
                    text: r.get(3)?,
                    text_hash: r.get(4)?,
                    source_turn_id: r.get(5)?,
                    supersedes_version: r.get(6)?,
                    change_reason: r.get(7)?,
                    diff_json: r.get(8)?,
                    created_by: r.get(9)?,
                    created_at: r.get(10)?,
                })
            }) {
                Ok(m) => Ok(Some(m)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await
    }

    /// Update a memory's text and record a new version.
    ///
    /// Inserts a new row in `memory_versions` with the new text, diff from the
    /// previous version, change_reason, and created_by. Updates the `memories`
    /// row text and updated_at. Returns the new MemoryRow.
    pub async fn update_memory(
        &self,
        memory_id: i64,
        new_text: &str,
        change_reason: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<MemoryRow> {
        self.update_memory_with_provenance(memory_id, new_text, None, change_reason, created_by)
            .await
    }

    /// Update text while optionally re-deriving trust from the update's
    /// provenance. Trust is monotonic: an untrusted row never becomes trusted
    /// merely because a later edit is attributed to a trusted source.
    pub async fn update_memory_with_provenance(
        &self,
        memory_id: i64,
        new_text: &str,
        source_kind: Option<&str>,
        change_reason: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<MemoryRow> {
        let new_text_owned = new_text.to_string();
        let new_hash = crate::util::sha256_hex(&new_text_owned);
        let reason = change_reason.map(|s| s.to_string());
        let by = created_by.map(|s| s.to_string());
        let source_kind = source_kind.map(|s| s.to_string());

        self.write(move |conn| {
            // Get current version number
            let current_version: i64 = conn.query_row(
                "SELECT COALESCE(MAX(version), 0) FROM memory_versions WHERE memory_id = ?1",
                [memory_id],
                |r| r.get(0),
            )?;

            // Get previous text for diff
            let prev_text: Option<String> = conn.query_row(
                "SELECT text FROM memory_versions WHERE memory_id = ?1 AND version = ?2",
                [memory_id, current_version],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();

            // Compute diff JSON
            let diff_json = prev_text.as_ref().map(|old| {
                let diff = compute_line_diff(old, &new_text_owned);
                serde_json::json!({
                    "old_hash": crate::util::sha256_hex(old),
                    "new_hash": new_hash,
                    "added_lines": diff.added,
                    "removed_lines": diff.removed,
                    "changed": !diff.added.is_empty() || !diff.removed.is_empty(),
                })
                .to_string()
            });

            let now = crate::util::SystemClock.now_millis();
            let new_version = current_version + 1;

            // Insert new version
            conn.execute(
                "INSERT INTO memory_versions (memory_id, version, text, text_hash, source_turn_id,
                        supersedes_version, change_reason, diff_json, created_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    memory_id,
                    new_version,
                    &new_text_owned,
                    &new_hash,
                    if current_version > 0 { Some(current_version) } else { None },
                    reason.as_deref(),
                    diff_json.as_deref(),
                    by.as_deref(),
                    now,
                ],
            )?;

            // Preserve or conservatively downgrade trust; never upgrade an
            // existing untrusted row. A provenance-bearing update also keeps
            // the sqlite-vec metadata mirror aligned with the canonical row.
            let (current_trust, current_source_kind): (String, String) = conn.query_row(
                "SELECT trust, source_kind FROM memories WHERE id = ?1",
                [memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let trust = source_kind
                .as_deref()
                .map(|kind| trust_after_source(&current_trust, kind))
                .unwrap_or_else(|| current_trust.as_str());
            let source_kind = match source_kind.as_deref() {
                Some(_) if current_trust == "untrusted" => current_source_kind.as_str(),
                Some(kind) => kind,
                None => current_source_kind.as_str(),
            };
            let trust_code = match trust {
                "trusted" => 0,
                "untrusted" => 1,
                "system" => 2,
                _ => unreachable!(),
            };
            conn.execute(
                "UPDATE memories SET text = ?1, text_hash = ?2, trust = ?3,
                 source_kind = ?4, updated_at = ?5 WHERE id = ?6",
                rusqlite::params![&new_text_owned, &new_hash, trust, source_kind, now, memory_id],
            )?;
            conn.execute(
                "UPDATE vec_memories SET trust = ?1 WHERE rowid = ?2",
                rusqlite::params![trust_code, memory_id],
            )?;

            // Fetch the updated row
            let row = conn.query_row(
                "SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at, summary_text, summary_tokens
                 FROM memories WHERE id = ?1",
                [memory_id],
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

            Ok(row)
        })
        .await
    }

    /// Rollback a memory to a specific version.
    ///
    /// Creates a new version whose text matches the target version, with
    /// `change_reason` recording the rollback. The original version chain
    /// is preserved. Returns the new MemoryRow.
    pub async fn rollback_memory(
        &self,
        memory_id: i64,
        target_version: i64,
        created_by: Option<&str>,
    ) -> Result<MemoryRow> {
        // Fetch the target version
        let target = self
            .get_memory_version(memory_id, target_version)
            .await?
            .ok_or_else(|| {
                Error::InvalidInput(format!(
                    "version {} not found for memory {}",
                    target_version, memory_id
                ))
            })?;

        let reason = format!("rollback to version {}", target_version);
        let new_text = target.text.clone();

        self.update_memory(memory_id, &new_text, Some(&reason), created_by)
            .await
    }

    /// Soft-deprecate a memory: sets `status = 'deprecated'`, `deleted_at = now`.
    /// Memory is excluded from recall (hard filter already excludes deprecated).
    /// Recoverable via `restore_memory`.
    ///
    /// Also writes a `forget_audit` row (action = 'deprecate').
    pub async fn deprecate_memory(&self, memory_id: i64, deleted_by: Option<&str>) -> Result<()> {
        self.deprecate_with_action(memory_id, deleted_by, "deprecate", None)
            .await
    }

    /// TTL reaper's soft-deprecate: identical state change, but the audit row
    /// records `ttl_deprecate` so automated retention actions are
    /// distinguishable from manual ones in the ledger (issue 0054).
    pub async fn deprecate_for_ttl(&self, memory_id: i64) -> Result<()> {
        self.deprecate_with_action(
            memory_id,
            Some("ttl-reaper"),
            "ttl_deprecate",
            Some("TTL expired"),
        )
        .await
    }

    async fn deprecate_with_action(
        &self,
        memory_id: i64,
        deleted_by: Option<&str>,
        action: &'static str,
        reason: Option<&str>,
    ) -> Result<()> {
        let now = crate::util::SystemClock.now_millis();
        let _agent_id = self.inner.agent_id.clone();
        let deleted_by_owned = deleted_by.map(|s| s.to_string());
        let reason_owned = reason.map(|s| s.to_string());
        self.write(move |conn| {
            // Fetch public_id before update for the audit row
            let public_id: Option<String> = conn
                .query_row(
                    "SELECT public_id FROM memories WHERE id = ?1",
                    [memory_id],
                    |r| r.get(0),
                )
                .optional()?;
            conn.execute(
                "UPDATE memories SET status = 'deprecated', deleted_at = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![now, now, memory_id],
            )?;
            // Write audit row
            if let Some(pid) = public_id {
                conn.execute(
                    "INSERT INTO forget_audit (action, selector_json, memory_ids, requester, reason, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        action,
                        &format!("{{\"memory_id\":{}}}", memory_id),
                        &pid,
                        deleted_by_owned.as_deref().unwrap_or("agent"),
                        reason_owned.as_deref(),
                        now,
                    ],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Restore a soft-deprecated memory back to active.
    /// Also writes a `forget_audit` row (action = 'restore').
    pub async fn restore_memory(&self, memory_id: i64, restored_by: Option<&str>) -> Result<()> {
        let now = crate::util::SystemClock.now_millis();
        let _agent_id = self.inner.agent_id.clone();
        let restored_by_owned = restored_by.map(|s| s.to_string());
        self.write(move |conn| {
            let public_id: Option<String> = conn
                .query_row(
                    "SELECT public_id FROM memories WHERE id = ?1",
                    [memory_id],
                    |r| r.get(0),
                )
                .optional()?;
            conn.execute(
                "UPDATE memories SET status = 'active', deleted_at = NULL, updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now, memory_id],
            )?;
            if let Some(pid) = public_id {
                conn.execute(
                    "INSERT INTO forget_audit (action, selector_json, memory_ids, requester, reason, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        "restore",
                        &format!("{{\"memory_id\":{}}}", memory_id),
                        &pid,
                        restored_by_owned.as_deref().unwrap_or("agent"),
                        None::<&str>,
                        now,
                    ],
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Hard purge: deletes the memory and every row that references it
    /// (`memory_versions`, `memory_links` both directions, `recall_items`,
    /// `pins`, the `vec_memories` vector; FTS is synced by trigger), verifies
    /// zero survivors on every table, runs `VACUUM`, and records the outcome
    /// in an append-only tombstone + `forget_audit` row (action =
    /// 'hard_delete').
    ///
    /// Verification is fail-closed: if any per-table count is non-zero after
    /// the deletes, the whole purge rolls back and the error names the table.
    pub async fn hard_purge_memory(
        &self,
        memory_id: i64,
        deleted_by: Option<&str>,
        reason: Option<&str>,
    ) -> Result<()> {
        self.hard_purge_with_action(memory_id, deleted_by, reason, "hard_delete")
            .await
    }

    /// TTL reaper's post-grace purge: identical mechanics to
    /// [`StoreHandle::hard_purge_memory`], but the audit row records
    /// `ttl_purge` so automated retention deletions are distinguishable from
    /// manual ones in the ledger (issue 0054).
    pub async fn hard_purge_for_ttl(&self, memory_id: i64) -> Result<()> {
        self.hard_purge_with_action(
            memory_id,
            Some("ttl-reaper"),
            Some("TTL grace period expired"),
            "ttl_purge",
        )
        .await
    }

    async fn hard_purge_with_action(
        &self,
        memory_id: i64,
        deleted_by: Option<&str>,
        reason: Option<&str>,
        action: &'static str,
    ) -> Result<()> {
        let now = crate::util::SystemClock.now_millis();
        let agent_id = self.inner.agent_id.clone();
        let deleted_by_owned = deleted_by.map(|s| s.to_string());
        let reason_owned = reason.map(|s| s.to_string());

        let (public_id, text_hash, rowcount_before, rowcount_after) = self
            .write(move |conn| {
            // Existence gate: an unknown id must fail *before* anything is
            // touched. `query_row` errors with QueryReturnedNoRows.
            let (public_id, text_hash): (String, String) = conn.query_row(
                "SELECT public_id, text_hash FROM memories WHERE id = ?1",
                [memory_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;

            // Per-table counts BEFORE, so the tombstone records what the purge
            // actually removed — not a hardcoded `1` for the memories row alone
            // (issue 0054: rowcounts are evidence, and evidence must be true).
            let count = |sql: &str| -> Result<i64> {
                Ok(conn.query_row(sql, [memory_id], |r| r.get(0))?)
            };
            let before_versions =
                count("SELECT COUNT(*) FROM memory_versions WHERE memory_id = ?1")?;
            let before_links = count(
                "SELECT COUNT(*) FROM memory_links WHERE from_memory_id = ?1 OR to_memory_id = ?1",
            )?;
            let before_recall_items =
                count("SELECT COUNT(*) FROM recall_items WHERE memory_id = ?1")?;
            let before_pins = count("SELECT COUNT(*) FROM pins WHERE memory_id = ?1")?;
            let before_vec = count("SELECT COUNT(*) FROM vec_memories WHERE rowid = ?1")?;
            let rowcount_before = 1
                + before_versions
                + before_links
                + before_recall_items
                + before_pins
                + before_vec;

            // The delete phase is one transaction: children first (FK order —
            // `recall_items` and `pins` reference `memories(id)`, so skipping
            // them made every recalled/pinned memory unpurgeable), then the
            // vector row, then the canonical row (its DELETE trigger syncs
            // fts_memories). Any failure rolls back everything.
            {
                let tx = conn.unchecked_transaction()?;
                conn.execute(
                    "DELETE FROM memory_links WHERE from_memory_id = ?1 OR to_memory_id = ?1",
                    [memory_id],
                )?;
                conn.execute(
                    "DELETE FROM memory_versions WHERE memory_id = ?1",
                    [memory_id],
                )?;
                conn.execute(
                    "DELETE FROM recall_items WHERE memory_id = ?1",
                    [memory_id],
                )?;
                conn.execute("DELETE FROM pins WHERE memory_id = ?1", [memory_id])?;
                conn.execute(
                    "DELETE FROM vec_memories WHERE rowid = ?1",
                    [memory_id],
                )?;
                conn.execute("DELETE FROM memories WHERE id = ?1", [memory_id])?;
                tx.commit()?;
            }

            // Fail-closed verification (issue 0054): the purge claim is only
            // true if *no* row survived on *any* referencing table. A non-zero
            // count aborts with the offending table named.
            let verify = |sql: &str, label: &str| -> Result<()> {
                let n: i64 = conn.query_row(sql, [memory_id], |r| r.get(0))?;
                if n != 0 {
                    return Err(Error::Storage(format!(
                        "purge verification failed: {n} row(s) survived on {label}"
                    )));
                }
                Ok(())
            };
            verify(
                "SELECT COUNT(*) FROM memories WHERE id = ?1",
                "memories",
            )?;
            verify(
                "SELECT COUNT(*) FROM memory_versions WHERE memory_id = ?1",
                "memory_versions",
            )?;
            verify(
                "SELECT COUNT(*) FROM memory_links WHERE from_memory_id = ?1 OR to_memory_id = ?1",
                "memory_links",
            )?;
            verify(
                "SELECT COUNT(*) FROM recall_items WHERE memory_id = ?1",
                "recall_items",
            )?;
            verify(
                "SELECT COUNT(*) FROM pins WHERE memory_id = ?1",
                "pins",
            )?;
            verify(
                "SELECT COUNT(*) FROM vec_memories WHERE rowid = ?1",
                "vec_memories",
            )?;
            // The FTS index is trigger-synced, but verify it too: a silent
            // search hit on a purged memory would be a leak.
            verify(
                "SELECT COUNT(*) FROM fts_memories WHERE rowid = ?1",
                "fts_memories",
            )?;
            let rowcount_after: i64 = 0;

            Ok((public_id, text_hash, rowcount_before, rowcount_after))
        })
        .await?;

        // VACUUM reclaims the freed pages. It cannot run inside a transaction,
        // so it runs after the delete phase committed; the duration is real
        // wall-clock, recorded in the tombstone (0054: no fake zeros).
        let vacuum_started = std::time::Instant::now();
        self.write(|conn| {
            conn.execute_batch("VACUUM;")?;
            Ok(())
        })
        .await?;
        let vacuum_duration_ms = vacuum_started.elapsed().as_millis() as i64;
        // Reclaim the WAL space the purge freed; its return value is
        // operational detail, not part of the purge contract.
        self.wal_checkpoint().await?;

        // Append the tombstone + audit row after success — append-only
        // evidence that the purge happened and what it removed.
        self.write(move |conn| {
            conn.execute(
                "INSERT INTO tombstones (public_id, agent_id, memory_id, text_hash, deleted_at, deleted_by, reason, rowcount_before, rowcount_after, vacuum_duration_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    &public_id,
                    &agent_id,
                    memory_id,
                    &text_hash,
                    now,
                    deleted_by_owned.as_deref(),
                    reason_owned.as_deref(),
                    rowcount_before,
                    rowcount_after,
                    vacuum_duration_ms,
                ],
            )?;

            conn.execute(
                "INSERT INTO forget_audit (action, selector_json, memory_ids, requester, reason, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    action,
                    &format!("{{\"memory_id\":{}}}", memory_id),
                    &public_id,
                    deleted_by_owned.as_deref().unwrap_or("agent"),
                    reason_owned.as_deref(),
                    now,
                ],
            )?;

            Ok(())
        })
        .await
    }

    /// Schema version of the opened database.
    pub async fn schema_version(&self) -> Result<i64> {
        self.read(schema::user_version).await
    }

    /// sqlite-vec version string (e.g. "v0.1.9").
    pub async fn vec_version(&self) -> Result<String> {
        self.read(vecext::verify).await
    }

    /// `PRAGMA integrity_check` result.
    pub async fn integrity(&self) -> Result<String> {
        self.read(schema::integrity_check).await
    }

    /// Supported schema version (compile-time constant).
    pub const SUPPORTED_SCHEMA: i64 = SCHEMA_VERSION;

    /// Run WAL checkpoint to prevent WAL file from growing unbounded.
    ///
    /// Uses `TRUNCATE` mode to reset the WAL file after checkpointing.
    /// Should be called periodically (e.g., via a maintenance job).
    pub async fn wal_checkpoint(&self) -> Result<String> {
        self.write(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok("ok".to_string())
        })
        .await
    }

    /// Write a consistent snapshot of the database to `out` via `VACUUM INTO`
    /// (v0.1.0 issue 0015, shipped in v0.1.1).
    ///
    /// The snapshot is taken through the single writer, so it includes every
    /// committed WAL frame (no checkpoint needed). The copy is then verified:
    /// `PRAGMA integrity_check` must report `ok` and the row counts of the
    /// core tables must match the live database.
    pub async fn snapshot_to(&self, out: &Path) -> Result<SnapshotReport> {
        if out.exists() {
            return Err(Error::InvalidInput(format!(
                "snapshot target {} already exists; remove it first",
                out.display()
            )));
        }
        let out_str = out
            .to_str()
            .ok_or_else(|| Error::InvalidInput("snapshot path is not valid UTF-8".into()))?
            .to_string();

        // VACUUM INTO writes a clean, defragmented copy; it must run on the
        // writer connection to serialize against concurrent writes.
        self.write(move |conn| {
            conn.execute("VACUUM INTO ?1", rusqlite::params![out_str])?;
            Ok(())
        })
        .await?;

        // Verify the snapshot before claiming success.
        let snap = Connection::open_with_flags(out, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| Error::Storage(format!("cannot open snapshot {}: {e}", out.display())))?;
        let integrity = schema::integrity_check(&snap)?;
        let snapshot_counts = table_counts(&snap)?;
        let live_counts = self.read(table_counts).await?;

        if snapshot_counts != live_counts {
            return Err(Error::Storage(format!(
                "snapshot verification failed: table counts differ \
                 (snapshot {snapshot_counts:?} vs live {live_counts:?})"
            )));
        }

        let bytes = std::fs::metadata(out)
            .map_err(|e| Error::Storage(format!("cannot stat snapshot: {e}")))?
            .len();

        tracing::info!(snapshot = %out.display(), bytes, "database snapshot written");
        Ok(SnapshotReport {
            path: out.to_path_buf(),
            bytes,
            integrity,
            tables: snapshot_counts,
        })
    }

    /// Evict old entries from the embeddings cache.
    ///
    /// Removes entries older than `max_age` that have been used less than
    /// `min_use_count` times. Returns the number of evicted entries.
    /// Should be called periodically to prevent unbounded cache growth.
    pub async fn evict_embeddings_cache(
        &self,
        max_age_millis: i64,
        min_use_count: i64,
        limit: i64,
    ) -> Result<i64> {
        self.write(move |conn| {
            let now = crate::util::SystemClock.now_millis();
            let cutoff = now - max_age_millis;

            let deleted = conn.execute(
                "DELETE FROM embeddings_cache
                 WHERE last_used_at < ?1
                   AND use_count < ?2
                 ORDER BY last_used_at ASC
                 LIMIT ?3",
                rusqlite::params![cutoff, min_use_count, limit],
            )?;
            Ok(deleted as i64)
        })
        .await
    }

    /// List all tombstone rows (hard-purged memories).
    pub async fn list_tombstones(&self) -> Result<Vec<TombstoneRow>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, public_id, agent_id, memory_id, text_hash, deleted_at,
                        deleted_by, reason, rowcount_before, rowcount_after, vacuum_duration_ms
                 FROM tombstones ORDER BY id DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(TombstoneRow {
                    id: r.get(0)?,
                    public_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    memory_id: r.get(3)?,
                    text_hash: r.get(4)?,
                    deleted_at: r.get(5)?,
                    deleted_by: r.get(6)?,
                    reason: r.get(7)?,
                    rowcount_before: r.get(8)?,
                    rowcount_after: r.get(9)?,
                    vacuum_duration_ms: r.get(10).unwrap_or(0),
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// List forget_audit rows, optionally filtered by action and/or since a timestamp.
    pub async fn list_forget_audit(
        &self,
        action: Option<String>,
        since: Option<i64>,
        limit: Option<i64>,
    ) -> Result<Vec<ForgetAuditRow>> {
        self.read(move |conn| {
            let mut sql = String::from(
                "SELECT id, action, selector_json, memory_ids, requester, reason, created_at
                 FROM forget_audit WHERE 1 = 1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(a) = &action {
                sql.push_str(" AND action = ?");
                params.push(Box::new(a.clone()));
            }
            if let Some(s) = since {
                sql.push_str(" AND created_at >= ?");
                params.push(Box::new(s));
            }
            sql.push_str(" ORDER BY id DESC");
            if let Some(l) = limit {
                sql.push_str(&format!(" LIMIT {}", l));
            }
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |r| {
                Ok(ForgetAuditRow {
                    id: r.get(0)?,
                    action: r.get(1)?,
                    selector_json: r.get(2)?,
                    memory_ids: r.get(3)?,
                    requester: r.get(4)?,
                    reason: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Find orphaned memory_versions rows (not referenced by any memories row).
    /// Returns the count of deleted rows.
    pub async fn delete_orphaned_versions(&self) -> Result<i64> {
        self.write(move |conn| {
            let n = conn.execute(
                "DELETE FROM memory_versions
                 WHERE memory_id NOT IN (SELECT id FROM memories)",
                [],
            )?;
            Ok(n as i64)
        })
        .await
    }

    /// Find memories eligible for dedup consolidation.
    /// Returns (memory_id, public_id, tier, text, text_hash) for all active memories.
    pub async fn all_active_memories_for_dedup(
        &self,
    ) -> Result<Vec<(i64, String, String, String, String, Option<Vec<u8>>)>> {
        self.read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.public_id, m.tier, m.text, m.text_hash, v.embedding
                 FROM memories m
                 LEFT JOIN vec_memories v ON v.rowid = m.id
                 WHERE m.status = 'active'
                 ORDER BY m.tier, m.id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Update dedup_cluster_id for a memory.
    pub async fn set_dedup_cluster(&self, memory_id: i64, cluster_id: &str) -> Result<()> {
        let now = crate::util::SystemClock.now_millis();
        let cid = cluster_id.to_string();
        self.write(move |conn| {
            conn.execute(
                "UPDATE memories SET dedup_cluster_id = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![&cid, now, memory_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Bump ref_count and last_referenced_at for a memory (dedup consolidation).
    pub async fn bump_ref_count(&self, memory_id: i64) -> Result<()> {
        let now = crate::util::SystemClock.now_millis();
        self.write(move |conn| {
            conn.execute(
                "UPDATE memories SET ref_count = ref_count + 1, last_referenced_at = ?1
                 WHERE id = ?2",
                rusqlite::params![now, memory_id],
            )?;
            Ok(())
        })
        .await
    }

    /// List memories by tier, optionally filtered by status.
    pub async fn list_memories(
        &self,
        tier: Option<String>,
        status: Option<String>,
        limit: Option<i64>,
    ) -> Result<Vec<MemoryRow>> {
        self.read(move |conn| {
            let mut sql = String::from(
                "SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at,
                        summary_text, summary_tokens
                 FROM memories WHERE 1 = 1",
            );
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if let Some(t) = &tier {
                sql.push_str(" AND tier = ?");
                params.push(Box::new(t.clone()));
            }
            if let Some(s) = &status {
                sql.push_str(" AND status = ?");
                params.push(Box::new(s.clone()));
            }
            sql.push_str(" ORDER BY id");
            if let Some(l) = limit {
                sql.push_str(&format!(" LIMIT {}", l));
            }
            let mut stmt = conn.prepare(&sql)?;
            let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), |r| {
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
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Find active memories whose TTL has expired.
    ///
    /// `ttl_days` is a function of tier; the caller passes per-tier TTLs
    /// (0 = never expires). Returns (memory_id, public_id, tier) triples.
    pub async fn expired_memories(
        &self,
        now_millis: i64,
        ttl_working: i64,
        ttl_episodic: i64,
        ttl_semantic: i64,
        ttl_procedural: i64,
    ) -> Result<Vec<(i64, String, String)>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.public_id, m.tier
                 FROM memories m
                 WHERE m.status = 'active'
                   AND (
                       (m.tier = 'working' AND ?1 > 0 AND m.created_at + ?1 * 86400000 < ?2)
                    OR (m.tier = 'episodic' AND ?3 > 0 AND m.created_at + ?3 * 86400000 < ?2)
                    OR (m.tier = 'semantic'  AND ?4 > 0 AND m.created_at + ?4 * 86400000 < ?2)
                    OR (m.tier = 'procedural' AND ?5 > 0 AND m.created_at + ?5 * 86400000 < ?2)
                   )
                 ORDER BY m.id",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![
                    ttl_working,
                    now_millis,
                    ttl_episodic,
                    ttl_semantic,
                    ttl_procedural
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Find deprecated memories older than the grace period (eligible for hard purge).
    pub async fn deprecated_past_grace(&self, grace_millis: i64) -> Result<Vec<(i64, String)>> {
        self.read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, public_id
                 FROM memories
                 WHERE status = 'deprecated'
                   AND deleted_at IS NOT NULL
                   AND deleted_at + ?1 < ?2
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(
                rusqlite::params![grace_millis, crate::util::SystemClock.now_millis()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
        .await
    }

    /// Get embeddings cache statistics.
    pub async fn embeddings_cache_stats(&self) -> Result<(i64, i64, i64)> {
        self.read(|conn| {
            let row = conn.query_row(
                "SELECT
                    COUNT(*) as count,
                    COALESCE(SUM(use_count), 0) as total_uses,
                    COALESCE(MAX(last_used_at), 0) as last_used
                 FROM embeddings_cache",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?;
            Ok(row)
        })
        .await
    }
}

/// Advisory single-process lock: one writer per database file (D11).
///
/// A `<db>.lock` sidecar is held with an exclusive non-blocking `flock` for
/// the lifetime of the [`Store`]. The kernel releases the flock when the
/// holder dies, so a lock left behind by a SIGKILLed process is reclaimed
/// automatically on the next open (previously a dead holder bricked the
/// database until the file was deleted by hand). The sidecar also records the
/// holding pid so a refused opener can name the offender. Defense-in-depth —
/// SQLite's own file locking remains the final arbiter; this exists to produce
/// a *clear error* instead of `SQLITE_BUSY`.
struct ProcessLock {
    /// The locked sidecar file; dropping it releases the flock.
    #[allow(dead_code)]
    file: std::fs::File,
    /// Sidecar path, kept for diagnostics and tests.
    #[allow(dead_code)]
    path: PathBuf,
    /// Pid recorded in the sidecar by this holder.
    #[allow(dead_code)]
    pid: u32,
}

impl ProcessLock {
    fn acquire(db_path: &Path) -> Result<Self> {
        // Append, don't replace the extension: "memory.db" -> "memory.db.lock".
        let lock_path = PathBuf::from(format!("{}.lock", db_path.display()));

        #[cfg(unix)]
        return Self::acquire_flock(db_path, &lock_path);

        #[cfg(not(unix))]
        return Self::acquire_o_excl(db_path, &lock_path);
    }

    /// flock-based acquisition (unix). The kernel drops the lock when the
    /// holder dies, so stale locks self-heal on the next open.
    #[cfg(unix)]
    fn acquire_flock(db_path: &Path, lock_path: &Path) -> Result<Self> {
        let file = open_lock_file(lock_path)?;
        match try_flock_exclusive(&file) {
            Ok(()) => {
                // We own the database. Record our pid so a refused opener can
                // name the holder in its error message.
                let pid = std::process::id();
                let mut file = file;
                {
                    use std::io::Write;
                    let _ = file.set_len(0);
                    let _ = writeln!(file, "pid {pid}");
                }
                tracing::debug!(lock = %lock_path.display(), "database lock acquired");
                Ok(Self {
                    file,
                    path: lock_path.to_path_buf(),
                    pid,
                })
            }
            Err(e) if is_would_block(&e) => Err(Error::DbLocked {
                path: db_path.display().to_string(),
                pid: read_holder_pid(lock_path),
            }),
            Err(e) => Err(Error::Storage(format!(
                "cannot lock {}: {e}",
                lock_path.display()
            ))),
        }
    }

    /// O_EXCL fallback for platforms without `flock` here. Weaker: a crashed
    /// holder leaves the sidecar behind and it must be removed by hand
    /// (documented in the runbook). Real targets are Linux; kept for
    /// portability only.
    #[cfg(not(unix))]
    fn acquire_o_excl(db_path: &Path, lock_path: &Path) -> Result<Self> {
        use std::io::Write;
        let token = format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64 ^ u64::from(std::process::id()))
                .unwrap_or(0)
        );
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                let _ = writeln!(f, "{token}");
                Ok(Self {
                    file: f,
                    path: lock_path.to_path_buf(),
                    pid: std::process::id(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::DbLocked {
                path: db_path.display().to_string(),
                pid: read_holder_pid(lock_path),
            }),
            Err(e) => Err(Error::Storage(format!(
                "cannot create lock file {}: {e}",
                lock_path.display()
            ))),
        }
    }
}

/// Open (creating if needed) the sidecar lock file without truncating it.
fn open_lock_file(lock_path: &Path) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| {
            Error::Storage(format!(
                "cannot open lock file {}: {e}",
                lock_path.display()
            ))
        })
}

/// Take an exclusive, non-blocking `flock` on an open sidecar file.
#[cfg(unix)]
fn try_flock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: flock(2) on a valid, open file descriptor; no memory involved.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn is_would_block(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock || e.raw_os_error() == Some(libc::EAGAIN)
}

/// Best-effort read of the holder pid recorded in the sidecar (`pid <n>`).
/// Returns 0 when the file is missing, empty, or from the legacy token format.
fn read_holder_pid(lock_path: &Path) -> u32 {
    std::fs::read_to_string(lock_path)
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.strip_prefix("pid ")
                    .and_then(|rest| rest.trim().parse::<u32>().ok())
            })
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(path: &Path) -> Config {
        Config {
            db_path: path.to_path_buf(),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn open_migrates_and_reports_versions() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreHandle::open(&test_config(&dir.path().join("m.db")), 2)
            .await
            .unwrap();
        assert_eq!(store.schema_version().await.unwrap(), SCHEMA_VERSION);
        assert!(store.vec_version().await.unwrap().starts_with('v'));
        assert_eq!(store.integrity().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn insert_and_get_memory_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreHandle::open(&test_config(&dir.path().join("m.db")), 2)
            .await
            .unwrap();

        let row = store
            .insert_memory(NewMemory {
                tier: "semantic".into(),
                kind: "fact".into(),
                text: "User prefers dark mode".into(),
                source_kind: "user".into(),
            })
            .await
            .unwrap();

        let got = store
            .get_memory(row.public_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.text, "User prefers dark mode");
        assert_eq!(got.status, "active");

        let missing = store.get_memory("no-such-id".into()).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn duplicate_open_is_blocked_by_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.db");
        let _first = StoreHandle::open(&test_config(&path), 1).await.unwrap();

        let err = StoreHandle::open(&test_config(&path), 1).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("one process per database"), "got: {msg}");
        assert!(msg.contains("pid"), "got: {msg}");
    }

    #[tokio::test]
    async fn concurrent_writes_serialize_through_writer() {
        let dir = tempfile::tempdir().unwrap();
        let store = StoreHandle::open(&test_config(&dir.path().join("m.db")), 4)
            .await
            .unwrap();

        let mut tasks = Vec::new();
        for i in 0..16 {
            let s = store.clone();
            tasks.push(tokio::spawn(async move {
                s.insert_memory(NewMemory {
                    tier: "working".into(),
                    kind: "note".into(),
                    text: format!("note {i}"),
                    source_kind: "user".into(),
                })
                .await
                .unwrap()
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let total: i64 = store
            .memory_counts()
            .await
            .unwrap()
            .iter()
            .map(|(_, n)| *n)
            .sum();
        assert_eq!(total, 16);
    }

    #[tokio::test]
    async fn duplicate_open_reports_holding_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pid.db");
        let first = StoreHandle::open(&test_config(&path), 1).await.unwrap();

        // The sidecar records the holder pid for error reporting.
        let lock_path = dir.path().join("pid.db.lock");
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let recorded: u32 = content
            .lines()
            .find_map(|l| l.strip_prefix("pid ").and_then(|r| r.trim().parse().ok()))
            .expect("lock file records the holder pid");
        assert_eq!(recorded, std::process::id());

        let err = StoreHandle::open(&test_config(&path), 1).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("one process per database"), "got: {msg}");
        assert!(
            msg.contains(&format!("pid {recorded}")),
            "must name the real holding pid, got: {msg}"
        );
        drop(first);
    }

    #[tokio::test]
    async fn stale_lock_from_dead_holder_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.db");

        // Simulate a SIGKILLed holder: a sidecar exists but nobody holds the
        // flock. The old implementation refused every later open forever.
        std::fs::write(dir.path().join("stale.db.lock"), "pid 999999999\n").unwrap();

        let store = StoreHandle::open(&test_config(&path), 1).await.unwrap();
        assert!(store.memory_counts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn dropping_the_store_releases_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relock.db");

        {
            let _first = StoreHandle::open(&test_config(&path), 1).await.unwrap();
            // Drop releases the flock; the sidecar file may remain.
        }

        // Reopen in the same process must succeed after clean shutdown.
        let _again = StoreHandle::open(&test_config(&path), 1).await.unwrap();
    }
}
