//! Schema and migrations (issue 0006).
//!
//! Migrations are a list of `(version, sql)` applied in order, tracked by
//! `PRAGMA user_version`. Applying twice is a no-op. Schema v1 contains the
//! full table set so later versions extend, never break.

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Highest schema version this binary understands.
pub const SCHEMA_VERSION: i64 = 3;

/// Ordered migration list. Index i-1 upgrades to version i.
pub const MIGRATIONS: &[&str] = &[
    // v1 — full foundation schema (part 1: agents, meta, sessions, turns).
    r#"
    CREATE TABLE IF NOT EXISTS agents (
        id         TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS sessions (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        public_id        TEXT NOT NULL UNIQUE,
        agent_id         TEXT NOT NULL DEFAULT 'default',
        started_at       INTEGER NOT NULL,
        ended_at         INTEGER,
        end_reason       TEXT CHECK (end_reason IN ('explicit','idle','forced')),
        status           TEXT NOT NULL DEFAULT 'open'
                         CHECK (status IN ('open','closed')),
        summary_memory_id INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_sessions_agent ON sessions(agent_id, started_at);

    CREATE TABLE IF NOT EXISTS turns (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id   INTEGER NOT NULL REFERENCES sessions(id),
        seq          INTEGER NOT NULL,
        role         TEXT NOT NULL CHECK (role IN ('user','assistant','system','tool')),
        content      TEXT NOT NULL,
        content_hash TEXT NOT NULL,
        created_at   INTEGER NOT NULL,
        tokens       INTEGER,
        UNIQUE (session_id, seq)
    );
    "#,
    // v1 — part 2: memories and friends.
    r#"
    CREATE TABLE IF NOT EXISTS memories (
        id                  INTEGER PRIMARY KEY AUTOINCREMENT,
        public_id           TEXT NOT NULL UNIQUE,
        agent_id            TEXT NOT NULL DEFAULT 'default',
        tier                TEXT NOT NULL
                            CHECK (tier IN ('working','episodic','semantic','procedural')),
        kind                TEXT NOT NULL
                            CHECK (kind IN ('fact','decision','preference','promise',
                                            'correction','lesson','summary','note')),
        text                TEXT NOT NULL,
        text_hash           TEXT NOT NULL,
        summary_text        TEXT,
        summary_tokens      INTEGER,
        importance_current  REAL NOT NULL DEFAULT 0.5 CHECK (importance_current BETWEEN 0 AND 1),
        confidence          REAL NOT NULL DEFAULT 1.0 CHECK (confidence BETWEEN 0 AND 1),
        status              TEXT NOT NULL DEFAULT 'active'
                            CHECK (status IN ('active','pending','deprecated','deleted')),
        trust               TEXT NOT NULL DEFAULT 'trusted'
                            CHECK (trust IN ('trusted','untrusted','system')),
        pinned              INTEGER NOT NULL DEFAULT 0,
        ref_count           INTEGER NOT NULL DEFAULT 0,
        last_referenced_at  INTEGER,
        expires_at          INTEGER,
        deleted_at          INTEGER,
        supersedes_id       INTEGER REFERENCES memories(id),
        superseded_by_id    INTEGER REFERENCES memories(id),
        source_kind         TEXT NOT NULL DEFAULT 'user'
                            CHECK (source_kind IN ('user','agent','tool','file','web','import')),
        source_ref          TEXT,
        session_id          INTEGER REFERENCES sessions(id),
        turn_id             INTEGER REFERENCES turns(id),
        extractor_version   TEXT,
        extractor_model     TEXT,
        embed_model         TEXT,
        embed_dim           INTEGER,
        embed_status        TEXT NOT NULL DEFAULT 'pending'
                            CHECK (embed_status IN ('pending','ok','skipped','failed')),
        dedup_cluster_id    INTEGER,
        created_at          INTEGER NOT NULL,
        updated_at          INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_memories_agent ON memories(agent_id, tier, status);
    CREATE INDEX IF NOT EXISTS idx_memories_text_hash ON memories(text_hash);
    CREATE INDEX IF NOT EXISTS idx_memories_expires ON memories(expires_at)
        WHERE expires_at IS NOT NULL;

    CREATE TABLE IF NOT EXISTS memory_versions (
        id                 INTEGER PRIMARY KEY AUTOINCREMENT,
        memory_id          INTEGER NOT NULL REFERENCES memories(id),
        version            INTEGER NOT NULL,
        text               TEXT NOT NULL,
        text_hash          TEXT NOT NULL,
        source_turn_id     INTEGER REFERENCES turns(id),
        supersedes_version INTEGER,
        change_reason      TEXT,
        diff_json          TEXT,
        created_by         TEXT,
        created_at         INTEGER NOT NULL,
        UNIQUE (memory_id, version)
    );

    CREATE TABLE IF NOT EXISTS memory_links (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        from_memory_id INTEGER NOT NULL REFERENCES memories(id),
        to_memory_id   INTEGER NOT NULL REFERENCES memories(id),
        kind           TEXT NOT NULL
                       CHECK (kind IN ('supersedes','derived_from','contradicts',
                                       'refines','summarizes')),
        created_at     INTEGER NOT NULL,
        UNIQUE (from_memory_id, to_memory_id, kind)
    );

    CREATE TABLE IF NOT EXISTS embeddings_cache (
        content_hash TEXT NOT NULL,
        embed_model  TEXT NOT NULL,
        dim          INTEGER NOT NULL,
        vector       BLOB NOT NULL,
        created_at   INTEGER NOT NULL,
        last_used_at INTEGER NOT NULL,
        use_count    INTEGER NOT NULL DEFAULT 1,
        PRIMARY KEY (content_hash, embed_model)
    );
    "#,
    // v1 — part 3: jobs, ledger, recall audit, forget audit.
    r#"
    CREATE TABLE IF NOT EXISTS jobs (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        kind            TEXT NOT NULL
                        CHECK (kind IN ('extract','summarize','maintain','reembed','reeval')),
        payload_json    TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'queued'
                        CHECK (status IN ('queued','running','done','failed','dead')),
        attempts        INTEGER NOT NULL DEFAULT 0,
        max_attempts    INTEGER NOT NULL DEFAULT 3,
        run_after       INTEGER NOT NULL DEFAULT 0,
        locked_at       INTEGER,
        lock_owner      TEXT,
        last_error      TEXT,
        idempotency_key TEXT NOT NULL UNIQUE,
        created_at      INTEGER NOT NULL,
        updated_at      INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS jobs_dead (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        job_id       INTEGER NOT NULL REFERENCES jobs(id),
        kind         TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        error        TEXT NOT NULL,
        attempts     INTEGER NOT NULL,
        failed_at    INTEGER NOT NULL,
        reprocessed_at INTEGER
    );

    CREATE TABLE IF NOT EXISTS llm_calls (
        id                INTEGER PRIMARY KEY AUTOINCREMENT,
        purpose           TEXT NOT NULL
                          CHECK (purpose IN ('extract','summarize','embed','eval')),
        model             TEXT NOT NULL,
        prompt_tokens     INTEGER NOT NULL,
        completion_tokens INTEGER NOT NULL,
        total_tokens      INTEGER NOT NULL,
        latency_ms        INTEGER NOT NULL,
        ok                INTEGER NOT NULL,
        cost_usd_est      REAL NOT NULL,
        created_at        INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_llm_calls_time ON llm_calls(created_at);

    CREATE TABLE IF NOT EXISTS token_ledger (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        session_id     INTEGER REFERENCES sessions(id),
        budget         INTEGER NOT NULL,
        tokens_used    INTEGER NOT NULL,
        tier_split_json TEXT NOT NULL,
        items_injected INTEGER NOT NULL,
        items_dropped  INTEGER NOT NULL,
        created_at     INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS recalls (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        query_hash  TEXT NOT NULL,
        query_text  TEXT NOT NULL,
        k           INTEGER NOT NULL,
        budget      INTEGER NOT NULL,
        candidates  INTEGER NOT NULL,
        injected    INTEGER NOT NULL,
        dropped     INTEGER NOT NULL,
        top_score   REAL,
        no_hit      INTEGER NOT NULL DEFAULT 0,
        latency_ms  INTEGER,
        session_id  INTEGER REFERENCES sessions(id),
        created_at  INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS recall_items (
        id              INTEGER PRIMARY KEY AUTOINCREMENT,
        recall_id       INTEGER NOT NULL REFERENCES recalls(id),
        memory_id       INTEGER NOT NULL REFERENCES memories(id),
        rank            INTEGER NOT NULL,
        score           REAL NOT NULL,
        components_json TEXT NOT NULL,
        injected        INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS pins (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        memory_id INTEGER NOT NULL UNIQUE REFERENCES memories(id),
        pinned_at INTEGER NOT NULL,
        by        TEXT NOT NULL DEFAULT 'agent',
        note      TEXT
    );

    CREATE TABLE IF NOT EXISTS forget_audit (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        action        TEXT NOT NULL
                      CHECK (action IN ('deprecate','hard_delete','expire','purge','restore','ttl_deprecate','ttl_purge')),
        selector_json TEXT NOT NULL,
        memory_ids    TEXT NOT NULL,
        requester     TEXT NOT NULL,
        reason        TEXT,
        cascade_json  TEXT,
        verification  TEXT NOT NULL DEFAULT 'pending'
                      CHECK (verification IN ('pending','pass','fail')),
        secure_erase  INTEGER NOT NULL DEFAULT 0,
        created_at    INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS tombstones (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        public_id        TEXT NOT NULL,
        agent_id         TEXT NOT NULL,
        memory_id        INTEGER,
        text_hash        TEXT NOT NULL,
        deleted_at       INTEGER NOT NULL,
        deleted_by       TEXT,
        reason           TEXT,
        rowcount_before  INTEGER NOT NULL,
        rowcount_after   INTEGER NOT NULL,
        vacuum_duration_ms INTEGER NOT NULL DEFAULT 0
    );
    "#,
];

/// FTS5 external-content table + sync triggers. Re-runnable repairs.
pub(crate) const FTS_DDL: &str = "
    CREATE VIRTUAL TABLE IF NOT EXISTS fts_memories USING fts5(
        text,
        content='memories',
        content_rowid='id',
        tokenize='unicode61'
    );

    CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
        INSERT INTO fts_memories(rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
        INSERT INTO fts_memories(fts_memories, rowid, text)
        VALUES ('delete', old.id, old.text);
    END;
    CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF text ON memories BEGIN
        INSERT INTO fts_memories(fts_memories, rowid, text)
        VALUES ('delete', old.id, old.text);
        INSERT INTO fts_memories(rowid, text) VALUES (new.id, new.text);
    END;
";

/// DDL for the vec0 virtual table; `dim` is fixed at creation and pinned in `meta`.
pub(crate) fn vec_ddl(dim: u32) -> String {
    format!(
        "
    CREATE VIRTUAL TABLE IF NOT EXISTS vec_memories USING vec0(
        embedding float[{dim}] distance_metric=cosine,
        tier TEXT,
        status INTEGER,
        trust INTEGER,
        kind TEXT,
        pinned INTEGER
    );
"
    )
}

/// Set durable, safe PRAGMAs on a fresh connection (issue 0005).
pub(crate) fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA secure_delete = ON;
        ",
    )?;
    Ok(())
}

/// PRAGMAs for read-only connections (no WAL switch allowed on readers).
pub(crate) fn apply_pragmas_read(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(())
}

/// Current user_version of the database.
pub fn user_version(conn: &Connection) -> Result<i64> {
    let v = conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))?;
    Ok(v)
}

/// Refuse databases written by a newer binary.
pub fn check_schema_supported(conn: &Connection) -> Result<()> {
    let v = user_version(conn)?;
    if v > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            db: v,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Bring the database up to [`SCHEMA_VERSION`]. Idempotent.
pub fn migrate(conn: &mut Connection) -> Result<()> {
    check_schema_supported(conn)?;

    loop {
        let v = user_version(conn)?;
        if v >= SCHEMA_VERSION {
            break;
        }
        let idx = v as usize;
        if idx >= MIGRATIONS.len() {
            return Err(Error::Storage(format!(
                "no migration for schema version {v} -> {}",
                v + 1
            )));
        }
        let sql = MIGRATIONS[idx];
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", v + 1)?;
        tx.commit()?;
        tracing::info!(from = v, to = v + 1, "applied schema migration");
    }

    // Virtual tables (FTS5, vec0) are version-independent repairs, safe to re-run.
    conn.execute_batch(FTS_DDL)?;
    Ok(())
}

/// Create the vec0 table for the configured embedding dimension and record it.
pub fn ensure_vec_table(conn: &Connection, dim: u32) -> Result<()> {
    conn.execute_batch(&vec_ddl(dim))?;
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('embed_dim', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![dim.to_string()],
    )?;
    Ok(())
}

/// All table names, for tests and `doctor`.
pub fn table_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type IN ('table','view')
         AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Run `PRAGMA integrity_check`.
pub fn integrity_check(conn: &Connection) -> Result<String> {
    let msg = conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::vecext;

    fn open_mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_pragmas(&conn).unwrap();
        conn
    }

    fn f32_blob(vals: &[f32]) -> Vec<u8> {
        vals.iter().flat_map(|f| f.to_le_bytes()).collect()
    }

    #[test]
    fn migration_sets_user_version_and_is_idempotent() {
        let mut conn = open_mem();
        migrate(&mut conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        migrate(&mut conn).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn all_core_tables_exist() {
        let mut conn = open_mem();
        migrate(&mut conn).unwrap();
        let tables = table_names(&conn).unwrap();
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
            assert!(tables.iter().any(|n| n == t), "missing table {t}");
        }
    }

    #[test]
    fn refuses_newer_schema() {
        let conn = open_mem();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
            .unwrap();
        let err = check_schema_supported(&conn).unwrap_err();
        assert!(err.to_string().contains("upgrade agos-memory"));
    }

    #[test]
    fn integrity_check_ok() {
        let mut conn = open_mem();
        migrate(&mut conn).unwrap();
        assert_eq!(integrity_check(&conn).unwrap(), "ok");
    }

    #[test]
    fn foreign_keys_enforced() {
        let mut conn = open_mem();
        migrate(&mut conn).unwrap();
        let fk = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn fts5_triggers_fire() {
        let mut conn = open_mem();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO memories (public_id, tier, kind, text, text_hash, created_at, updated_at)
             VALUES ('p1','semantic','fact','hello world','h1',1,1)",
            [],
        )
        .unwrap();
        let hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM fts_memories WHERE fts_memories MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }

    #[test]
    fn vec0_knn_smoke_with_metadata_filter() {
        let mut conn = open_mem();
        vecext::register().unwrap();
        migrate(&mut conn).unwrap();
        ensure_vec_table(&conn, 4).unwrap();

        let ins = |rowid: i64, v: Vec<u8>, tier: &str, status: i64, trust: i64| {
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'fact', 0)",
                rusqlite::params![rowid, v, tier, status, trust],
            )
            .unwrap();
        };
        // rowid 1 is 'semantic/active' and closest to the query vector.
        ins(1, f32_blob(&[1.0, 1.0, 1.0, 1.0]), "semantic", 0, 0);
        // rowid 2 would be closest but is deprecated (status 2): must be filtered
        // inside the KNN scan.
        ins(2, f32_blob(&[1.0, 1.0, 1.0, 1.0]), "semantic", 2, 0);
        ins(3, f32_blob(&[-1.0, -1.0, -1.0, -1.0]), "episodic", 0, 0);

        let query = f32_blob(&[1.0, 1.0, 1.0, 1.0]);
        let mut stmt = conn
            .prepare(
                "SELECT rowid, distance FROM vec_memories
                 WHERE embedding MATCH ?1 AND k = 5
                   AND status = 0 AND tier = 'semantic'
                 ORDER BY distance",
            )
            .unwrap();
        let rows: Vec<(i64, f64)> = stmt
            .query_map(rusqlite::params![query], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows.len(),
            1,
            "hard filter must exclude deprecated + other-tier rows"
        );
        assert_eq!(rows[0].0, 1);
        assert!(rows[0].1.abs() < 1e-5, "identical vectors, distance ~0");
    }

    #[test]
    fn ensure_vec_table_records_dim() {
        vecext::register().unwrap();
        let mut conn = open_mem();
        migrate(&mut conn).unwrap();
        ensure_vec_table(&conn, 384).unwrap();
        let dim: String = conn
            .query_row("SELECT value FROM meta WHERE key = 'embed_dim'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(dim, "384");
    }
}
