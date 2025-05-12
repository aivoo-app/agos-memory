//! Integration: schema migrations on a real database file (v0.1.0 acceptance).
//!
//! Complements the inline unit tests: these run against an on-disk database,
//! not `:memory:`.

use agos_memory::storage::schema::{self, SCHEMA_VERSION};
use agos_memory::storage::vecext;
use rusqlite::Connection;

/// Open an on-disk connection with the same PRAGMAs the store would set.
fn open_file(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).expect("open database file");
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;",
    )
    .expect("set pragmas");
    conn
}

#[test]
fn migration_is_idempotent_on_a_file_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    let mut conn = open_file(&path);

    schema::migrate(&mut conn).unwrap();
    assert_eq!(schema::user_version(&conn).unwrap(), SCHEMA_VERSION);

    // Second run must be a no-op, not an error.
    schema::migrate(&mut conn).unwrap();
    assert_eq!(schema::user_version(&conn).unwrap(), SCHEMA_VERSION);
}

#[test]
fn migrated_file_database_has_the_full_table_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tables.db");
    let mut conn = open_file(&path);
    schema::migrate(&mut conn).unwrap();

    let tables = schema::table_names(&conn).unwrap();
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
        "fts_memories",
    ] {
        assert!(
            tables.iter().any(|n| n == t),
            "missing table {t} in {tables:?}"
        );
    }
}

#[test]
fn database_from_a_newer_binary_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("future.db");
    let mut conn = open_file(&path);
    schema::migrate(&mut conn).unwrap();

    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 7)
        .unwrap();

    let err = schema::migrate(&mut conn).unwrap_err();
    assert!(
        err.to_string().contains("upgrade agos-memory"),
        "got: {err}"
    );

    // The standalone check agrees.
    let err = schema::check_schema_supported(&conn).unwrap_err();
    assert!(matches!(
        err,
        agos_memory::error::Error::SchemaTooNew { .. }
    ));
}

#[test]
fn fts5_triggers_fire_on_a_file_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fts.db");
    let mut conn = open_file(&path);
    schema::migrate(&mut conn).unwrap();

    conn.execute(
        "INSERT INTO memories (public_id, tier, kind, text, text_hash, created_at, updated_at)
         VALUES ('p1', 'semantic', 'fact', 'the launch codename is heron', 'h1', 1, 1)",
        [],
    )
    .unwrap();

    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM fts_memories WHERE fts_memories MATCH 'heron'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);

    // Updating the text keeps the index in sync via the update trigger.
    conn.execute(
        "UPDATE memories SET text = 'the launch codename is kestrel' WHERE public_id = 'p1'",
        [],
    )
    .unwrap();
    let hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM fts_memories WHERE fts_memories MATCH 'kestrel'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);
}

#[test]
fn sqlite_vec_registers_for_every_new_connection() {
    let dir = tempfile::tempdir().unwrap();

    // Registration is process-wide and idempotent; two fresh connections on
    // two different files must both see the extension.
    vecext::register().unwrap();
    for name in ["a.db", "b.db"] {
        let conn = open_file(&dir.path().join(name));
        let v = vecext::verify(&conn).unwrap();
        assert!(v.starts_with('v'), "vec_version() = {v}");
    }
}
