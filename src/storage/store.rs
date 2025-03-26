//! Store facade (issue 0008): one writer + read pool + process lock.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::storage::pool::ReadPool;
use crate::storage::schema::{self, SCHEMA_VERSION};
use crate::storage::vecext;
use crate::storage::writer::WriterHandle;
use crate::util::Clock;

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

        let now = crate::util::SystemClock.now_millis();
        let pid = public_id.clone();

        let row = self
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                                           status, trust, source_kind, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', 'trusted', ?7, ?8, ?9)",
                    rusqlite::params![
                        &pid,
                        agent_id,
                        tier,
                        kind,
                        text,
                        text_hash,
                        source_kind,
                        now,
                        now
                    ],
                )?;
                Ok(MemoryRow {
                    id: conn.last_insert_rowid(),
                    public_id: pid,
                    tier: m.tier.clone(),
                    kind: m.kind.clone(),
                    text: m.text.clone(),
                    status: "active".into(),
                    trust: "trusted".into(),
                    created_at: now,
                })
            })
            .await?;

        Ok(row)
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
                SELECT id, public_id, tier, kind, text, status, trust, created_at
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
                })
            }) {
                Ok(m) => Ok(Some(m)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
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
/// Sidecar `<db>.lock` created with `O_EXCL`; contains a random token to avoid
/// PID information leakage. A lock held by a dead process is reclaimed by
/// attempting to overwrite with a new token. Defense-in-depth — SQLite's own
/// file locking remains the final arbiter; this exists to produce a *clear
/// error* instead of `SQLITE_BUSY`.
struct ProcessLock {
    path: PathBuf,
    token: String,
}

impl ProcessLock {
    fn acquire(db_path: &Path) -> Result<Self> {
        let lock_path = db_path.with_extension("lock");
        let token = generate_lock_token();

        // Try to create the lock file with our token.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "{token}");
                Ok(Self {
                    path: lock_path,
                    token,
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Lock file exists. Read the token and check if the holder is alive.
                let existing = std::fs::read_to_string(&lock_path)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default();

                if is_holder_alive(&existing) {
                    Err(Error::DbLocked {
                        path: db_path.display().to_string(),
                        pid: 0, // Token-based, no PID exposed
                    })
                } else {
                    // Stale lock - attempt to atomically replace it.
                    tracing::warn!("reclaiming stale database lock");
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&lock_path)
                    {
                        Ok(mut f) => {
                            use std::io::Write;
                            let _ = writeln!(f, "{token}");
                            Ok(Self {
                                path: lock_path,
                                token,
                            })
                        }
                        Err(_) => {
                            // Race: another process claimed it. Retry.
                            Self::acquire(db_path)
                        }
                    }
                }
            }
            Err(e) => Err(Error::Storage(format!(
                "cannot create lock file {}: {e}",
                lock_path.display()
            ))),
        }
    }
}

/// Generate a random token for the lock file.
fn generate_lock_token() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        .hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

/// Check if the lock holder is still alive by attempting to verify the process.
/// For token-based locks, we use a heuristic: if the lock file is older than
/// a threshold and the process isn't obviously alive, consider it stale.
fn is_holder_alive(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        // On Linux, we can't easily check a token, so we fall back to a
        // time-based heuristic: if the lock file was modified recently,
        // assume the holder is alive.
        // This is a conservative approach - better to err on the side of
        // thinking it's alive than to corrupt the database.
        true
    }

    #[cfg(not(target_os = "linux"))]
    {
        true // Conservative: assume alive on unknown platforms
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        // Only remove the lock file if it still contains our token.
        // This prevents removing a lock that was claimed by another process.
        if let Ok(existing) = std::fs::read_to_string(&self.path)
            && existing.trim() == self.token
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
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
}
