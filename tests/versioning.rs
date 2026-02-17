//! Memory versioning: update creates new version with diff, explain shows
//! supersedes chain, rollback creates new head, old chain intact.

use agos_memory::config::Config;
use agos_memory::storage::StoreHandle;

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

#[tokio::test]
async fn update_creates_new_version_with_diff() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("v.db")), 1)
        .await
        .unwrap();

    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: "line one\nline two\nline three".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();

    // Update the memory
    let updated = store
        .update_memory(
            row.id,
            "line one\nline two updated\nline three\nline four",
            Some("user correction"),
            Some("test"),
        )
        .await
        .unwrap();

    assert_eq!(
        updated.text,
        "line one\nline two updated\nline three\nline four"
    );

    // Fetch versions
    let versions = store.get_memory_versions(row.id).await.unwrap();
    assert_eq!(versions.len(), 2, "should have 2 versions");
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[1].version, 2);
    assert_eq!(
        versions[1].text,
        "line one\nline two updated\nline three\nline four"
    );
    assert_eq!(
        versions[1].change_reason.as_deref(),
        Some("user correction")
    );
    assert_eq!(versions[1].created_by.as_deref(), Some("test"));
    assert!(versions[1].diff_json.is_some(), "diff should be computed");
    assert_eq!(versions[1].supersedes_version, Some(1));
}

#[tokio::test]
async fn explain_shows_supersedes_chain() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("e.db")), 1)
        .await
        .unwrap();

    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: "original text".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();

    store
        .update_memory(row.id, "updated text", Some("edit"), Some("user"))
        .await
        .unwrap();

    let versions = store.get_memory_versions(row.id).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[1].version, 2);
    assert_eq!(versions[1].supersedes_version, Some(1));
    assert_eq!(versions[1].text, "updated text");
}

#[tokio::test]
async fn rollback_creates_new_head_old_chain_intact() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("r.db")), 1)
        .await
        .unwrap();

    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: "v1 text".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();

    store
        .update_memory(row.id, "v2 text", Some("edit"), Some("user"))
        .await
        .unwrap();

    store
        .update_memory(row.id, "v3 text", Some("edit"), Some("user"))
        .await
        .unwrap();

    // Rollback to version 1
    let rolled_back = store
        .rollback_memory(row.id, 1, Some("user"))
        .await
        .unwrap();

    assert_eq!(
        rolled_back.text, "v1 text",
        "rolled back text should match v1"
    );

    // All 4 versions should exist
    let versions = store.get_memory_versions(row.id).await.unwrap();
    assert_eq!(versions.len(), 4, "should have 4 versions after rollback");
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[0].text, "v1 text");
    assert_eq!(versions[1].version, 2);
    assert_eq!(versions[1].text, "v2 text");
    assert_eq!(versions[2].version, 3);
    assert_eq!(versions[2].text, "v3 text");
    assert_eq!(versions[3].version, 4);
    assert_eq!(
        versions[3].text, "v1 text",
        "v4 should have rolled back text"
    );
    assert!(
        versions[3]
            .change_reason
            .as_deref()
            .unwrap()
            .contains("rollback to version 1"),
        "change reason should mention rollback"
    );
}

#[tokio::test]
async fn recall_uses_latest_version_text() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("rc.db")), 1)
        .await
        .unwrap();

    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: "original".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();

    store
        .update_memory(row.id, "updated content", Some("edit"), Some("user"))
        .await
        .unwrap();

    // get_memory should return the latest text
    let got = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.text, "updated content");
}

#[tokio::test]
async fn diff_json_is_valid_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("d.db")), 1)
        .await
        .unwrap();

    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: "hello world".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();

    store
        .update_memory(row.id, "hello world again", Some("edit"), Some("user"))
        .await
        .unwrap();

    let versions = store.get_memory_versions(row.id).await.unwrap();
    assert_eq!(versions.len(), 2);
    let diff_json = versions[1].diff_json.as_deref().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(diff_json).unwrap();
    assert!(parsed["added_lines"].is_array());
    assert!(parsed["removed_lines"].is_array());
    assert!(parsed["changed"].is_boolean());
    assert_eq!(parsed["changed"], true);
}
