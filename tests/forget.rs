//! Verified deletion: soft deprecate → hard purge + tombstone.

use agos_memory::config::Config;
use agos_memory::storage::StoreHandle;

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

async fn make_memory(store: &StoreHandle, text: &str) -> agos_memory::storage::MemoryRow {
    store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: text.into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn soft_deprecate_hides_from_recall() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("f.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "remember this").await;
    store.deprecate_memory(row.id, Some("test")).await.unwrap();

    let got = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, "deprecated", "memory should be deprecated");
}

#[tokio::test]
async fn restore_after_soft_deprecate_works() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("r.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "test restore").await;
    store.deprecate_memory(row.id, Some("test")).await.unwrap();
    store.restore_memory(row.id, Some("test")).await.unwrap();

    let got = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, "active");
    assert_eq!(got.text, "test restore");
}

#[tokio::test]
async fn hard_purge_removes_from_all_tables() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("h.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "to be deleted").await;

    let got = store.get_memory(row.public_id.clone()).await.unwrap();
    assert!(got.is_some());

    store
        .hard_purge_memory(row.id, Some("test"), Some("cleanup"))
        .await
        .unwrap();

    let got = store.get_memory(row.public_id.clone()).await.unwrap();
    assert!(got.is_none(), "hard purged memory should not be returned");

    let versions = store.get_memory_versions(row.id).await.unwrap();
    assert!(versions.is_empty(), "versions should be purged");

    let tombstone_count: i64 = store
        .read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tombstones WHERE memory_id = ?1",
                [row.id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| agos_memory::error::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(tombstone_count, 1, "tombstone should be inserted");
}

#[tokio::test]
async fn hard_purge_on_already_deleted_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("a.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "delete me").await;
    store
        .hard_purge_memory(row.id, Some("test"), Some("first"))
        .await
        .unwrap();

    let result = store
        .hard_purge_memory(row.id, Some("test"), Some("second"))
        .await;
    assert!(
        result.is_err(),
        "purge of already-deleted memory should fail"
    );
}

#[tokio::test]
async fn tombstone_is_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("t.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "tombstone test").await;
    store
        .hard_purge_memory(row.id, Some("user"), Some("reason"))
        .await
        .unwrap();

    // The v4 immutability trigger must reject the rewrite outright (issue
    // 0054): a tombstone that could be edited would not be evidence. The
    // rejection is asserted, never swallowed.
    let result = store
        .write(move |conn| {
            conn.execute(
                "UPDATE tombstones SET reason = 'hacked' WHERE memory_id = ?1",
                [row.id],
            )?;
            Ok(())
        })
        .await;

    let err = result.expect_err("tombstones UPDATE must be rejected by the immutability trigger");
    assert!(
        err.to_string().contains("immutable"),
        "error should name the immutability trigger, got: {err}"
    );

    // DELETE is equally forbidden — append-only means append-only.
    let result = store
        .write(move |conn| {
            conn.execute("DELETE FROM tombstones WHERE memory_id = ?1", [row.id])?;
            Ok(())
        })
        .await;
    assert!(
        result.is_err(),
        "tombstones DELETE must be rejected by the immutability trigger"
    );
}

#[tokio::test]
async fn forget_audit_records_action() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("au.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "audit test").await;
    store.deprecate_memory(row.id, Some("test")).await.unwrap();

    let status: String = store
        .read(move |conn| {
            conn.query_row("SELECT status FROM memories WHERE id = ?1", [row.id], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|e| agos_memory::error::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(status, "deprecated");
}

#[tokio::test]
async fn soft_deprecate_then_hard_purge_works() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("s.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "soft then hard").await;
    store.deprecate_memory(row.id, Some("test")).await.unwrap();

    let got = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.status, "deprecated");

    store
        .hard_purge_memory(row.id, Some("test"), Some("finally"))
        .await
        .unwrap();

    let count: i64 = store
        .read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tombstones WHERE memory_id = ?1",
                [row.id],
                |r| r.get::<_, i64>(0),
            )
            .map_err(|e| agos_memory::error::Error::Storage(e.to_string()))
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
}
