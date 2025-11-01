//! TTL reaper integration tests (issues 0044, 0047).

use agos_memory::config::Config;
use agos_memory::storage::StoreHandle;

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

async fn make_memory(
    store: &StoreHandle,
    text: &str,
    tier: &str,
) -> agos_memory::storage::MemoryRow {
    store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: tier.into(),
            kind: "fact".into(),
            text: text.into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn soft_deprecated_after_ttl() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("ttl.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "old memory", "working").await;

    // Simulate TTL expiry by setting created_at far in the past
    let old_time = 1i64; // epoch start
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![old_time, row.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    // Run TTL reaper with working TTL = 30 days
    let mut cfg = Config::default();
    cfg.memory.ttl_working_days = 30;
    let report = agos_memory::memory::run_ttl_reaper(&store, &cfg)
        .await
        .unwrap();
    assert_eq!(
        report.soft_deprecated, 1,
        "one memory should be soft-deprecated"
    );

    let got = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        got.status, "deprecated",
        "memory should be deprecated after TTL"
    );
}

#[tokio::test]
async fn hard_purge_after_grace() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("grace.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "old memory", "working").await;

    // Deprecate and set deleted_at far in past
    store.deprecate_memory(row.id, Some("test")).await.unwrap();
    let old_time = 1i64;
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET deleted_at = ?1 WHERE id = ?2",
                rusqlite::params![old_time, row.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    // Run TTL reaper with grace = 30 days
    let mut cfg = Config::default();
    cfg.memory.ttl_grace_days = 30;
    let report = agos_memory::memory::run_ttl_reaper(&store, &cfg)
        .await
        .unwrap();
    assert_eq!(
        report.hard_purged, 1,
        "one memory should be hard-purged after grace"
    );

    let got = store.get_memory(row.public_id.clone()).await.unwrap();
    assert!(got.is_none(), "memory should be purged");
}

#[tokio::test]
async fn configurable_per_tier() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("tier.db")), 1)
        .await
        .unwrap();

    // Create memories in different tiers
    let w = make_memory(&store, "working memory", "working").await;
    let e = make_memory(&store, "episodic memory", "episodic").await;
    let s = make_memory(&store, "semantic memory", "semantic").await;

    // Set all created_at to old
    let old_time = 1i64;
    for row in [&w, &e, &s] {
        let id = row.id;
        store
            .write(move |conn| {
                conn.execute(
                    "UPDATE memories SET created_at = ?1 WHERE id = ?2",
                    rusqlite::params![old_time, id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    // Only working and episodic have TTL; semantic never expires
    let mut cfg = Config::default();
    cfg.memory.ttl_working_days = 30;
    cfg.memory.ttl_episodic_days = 90;
    cfg.memory.ttl_semantic_days = 0; // never

    let report = agos_memory::memory::run_ttl_reaper(&store, &cfg)
        .await
        .unwrap();
    assert_eq!(
        report.soft_deprecated, 2,
        "working + episodic should be deprecated, semantic should not"
    );

    let s_got = store
        .get_memory(s.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        s_got.status, "active",
        "semantic should remain active (no TTL)"
    );
}

#[tokio::test]
async fn forget_audit_logs_ttl_actions() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("audit.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "audit test", "working").await;
    let old_time = 1i64;
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET created_at = ?1 WHERE id = ?2",
                rusqlite::params![old_time, row.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let mut cfg = Config::default();
    cfg.memory.ttl_working_days = 30;
    agos_memory::memory::run_ttl_reaper(&store, &cfg)
        .await
        .unwrap();

    let entries = store.list_forget_audit(None, None, None).await.unwrap();
    assert!(!entries.is_empty(), "audit entries should exist");
    assert!(
        entries.iter().any(|e| e.action == "deprecate"),
        "should have deprecate action"
    );
}

#[tokio::test]
async fn reaper_processes_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("batch.db")), 1)
        .await
        .unwrap();

    // Create 5 old working memories
    for i in 0..5 {
        let row = make_memory(&store, &format!("batch {i}"), "working").await;
        let old_time = 1i64;
        store
            .write(move |conn| {
                conn.execute(
                    "UPDATE memories SET created_at = ?1 WHERE id = ?2",
                    rusqlite::params![old_time, row.id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    let mut cfg = Config::default();
    cfg.memory.ttl_working_days = 30;
    let report = agos_memory::memory::run_ttl_reaper(&store, &cfg)
        .await
        .unwrap();
    assert_eq!(report.soft_deprecated, 5, "all 5 should be deprecated");
}
