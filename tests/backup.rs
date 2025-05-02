//! Integration: verified database snapshots (`VACUUM INTO`).

use agos_memory::config::Config;
use agos_memory::storage::{NewMemory, StoreHandle};
use rusqlite::Connection;

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

#[tokio::test]
async fn snapshot_is_verified_and_matches_the_live_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("live.db");
    let snap = dir.path().join("snap.db");

    let store = StoreHandle::open(&test_config(&db), 2).await.unwrap();
    for i in 0..5 {
        store
            .insert_memory(NewMemory {
                tier: "semantic".into(),
                kind: "fact".into(),
                text: format!("fact number {i}"),
                source_kind: "user".into(),
            })
            .await
            .unwrap();
    }

    let report = store.snapshot_to(&snap).await.unwrap();
    assert_eq!(report.path, snap);
    assert_eq!(report.integrity, "ok");
    assert!(report.bytes > 0);
    let memories = report
        .tables
        .iter()
        .find(|(t, _)| t == "memories")
        .expect("memories counted");
    assert_eq!(memories.1, 5);

    // The snapshot itself is a valid, openable database with the same rows.
    let conn =
        Connection::open_with_flags(&snap, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 5);
}

#[tokio::test]
async fn snapshot_captures_wal_pending_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("wal.db");
    let snap = dir.path().join("wal-snap.db");

    let store = StoreHandle::open(&test_config(&db), 2).await.unwrap();
    store
        .insert_memory(NewMemory {
            tier: "episodic".into(),
            kind: "note".into(),
            text: "committed but still in the WAL".into(),
            source_kind: "agent".into(),
        })
        .await
        .unwrap();
    // No explicit checkpoint: the write lives in the WAL frame. The snapshot
    // taken through the writer must include it.
    store.snapshot_to(&snap).await.unwrap();

    let conn =
        Connection::open_with_flags(&snap, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "snapshot must include committed WAL content");
}

#[tokio::test]
async fn snapshot_refuses_to_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("x.db")), 1)
        .await
        .unwrap();
    let out = dir.path().join("out.db");
    store.snapshot_to(&out).await.unwrap();

    let err = store.snapshot_to(&out).await.unwrap_err();
    assert!(err.to_string().contains("already exists"), "got: {err}");
}
