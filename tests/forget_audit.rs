//! Forget audit trail tests (issue 0046).

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
async fn every_forget_action_logs_audit() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("audit.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "audit me").await;

    // Soft deprecate
    store.deprecate_memory(row.id, Some("user1")).await.unwrap();

    // Restore
    store.restore_memory(row.id, Some("user2")).await.unwrap();

    // Hard purge
    store
        .hard_purge_memory(row.id, Some("user3"), Some("cleanup"))
        .await
        .unwrap();

    let entries = store.list_forget_audit(None, None, None).await.unwrap();
    assert!(entries.len() >= 3, "should have at least 3 audit entries");

    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    assert!(
        actions.contains(&"deprecate"),
        "should have deprecate action"
    );
    assert!(actions.contains(&"restore"), "should have restore action");
    assert!(
        actions.contains(&"hard_delete"),
        "should have hard_delete action"
    );
}

#[tokio::test]
async fn list_audit_filters_by_action() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("filter.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "filter test").await;
    store.deprecate_memory(row.id, Some("user")).await.unwrap();
    store.restore_memory(row.id, Some("user")).await.unwrap();

    let deprecate_only = store
        .list_forget_audit(Some("deprecate".to_string()), None, None)
        .await
        .unwrap();
    assert_eq!(
        deprecate_only.len(),
        1,
        "should only have deprecate entries"
    );
    assert_eq!(deprecate_only[0].action, "deprecate");
}

#[tokio::test]
async fn list_tombstones_shows_purged_memories() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("tomb.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "to tombstone").await;
    store
        .hard_purge_memory(row.id, Some("user"), Some("test"))
        .await
        .unwrap();

    let tombstones = store.list_tombstones().await.unwrap();
    assert_eq!(tombstones.len(), 1, "should have one tombstone");
    assert_eq!(tombstones[0].public_id, row.public_id);
}

#[tokio::test]
async fn audit_is_immutable() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("immutable.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "immutable test").await;
    store.deprecate_memory(row.id, Some("user")).await.unwrap();

    let entries_before = store.list_forget_audit(None, None, None).await.unwrap();
    let count_before = entries_before.len();

    // The v4 immutability trigger must reject the rewrite outright (issue
    // 0054): append-only evidence is enforced in the database, not by
    // convention, and the rejection is asserted — never swallowed.
    let result = store
        .write(move |conn| {
            conn.execute(
                "UPDATE forget_audit SET reason = 'hacked' WHERE id = ?1",
                [entries_before[0].id],
            )?;
            Ok(())
        })
        .await;

    let err = result.expect_err("forget_audit UPDATE must be rejected by the immutability trigger");
    assert!(
        err.to_string().contains("immutable"),
        "error should name the immutability trigger, got: {err}"
    );

    let entries_after = store.list_forget_audit(None, None, None).await.unwrap();
    assert_eq!(
        entries_after.len(),
        count_before,
        "audit count should not change"
    );
}
