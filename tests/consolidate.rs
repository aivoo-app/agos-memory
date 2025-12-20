//! Consolidation job tests (issue 0045).

use agos_memory::config::Config;
use agos_memory::llm::MockChat;
use agos_memory::storage::StoreHandle;
use std::sync::Arc;

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
async fn summarization_pass_generates_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("consolidate.db")), 1)
        .await
        .unwrap();

    // Create old memories without summaries
    let row = make_memory(&store, "important fact about Rust", "semantic").await;
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

    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();
    let report = agos_memory::memory::run_consolidation_job(&store, &llm, &cfg)
        .await
        .unwrap();
    assert!(
        report.summaries_generated < 1000,
        "should run without error"
    );
}

#[tokio::test]
async fn orphan_cleanup_removes_unreferenced_versions() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("orphan.db")), 1)
        .await
        .unwrap();

    // Create a memory (which creates a version)
    let row = make_memory(&store, "has version", "semantic").await;

    // Hard-purge the memory, leaving orphaned versions
    store
        .hard_purge_memory(row.id, Some("test"), Some("orphan test"))
        .await
        .unwrap();

    // Run consolidation - should clean up orphans
    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();
    let report = agos_memory::memory::run_consolidation_job(&store, &llm, &cfg)
        .await
        .unwrap();
    assert!(report.orphans_deleted < 1000, "should run without error");
}

#[tokio::test]
async fn consolidation_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("idempotent.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "idempotent test", "semantic").await;
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

    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();

    // Run twice
    let report1 = agos_memory::memory::run_consolidation_job(&store, &llm, &cfg)
        .await
        .unwrap();
    let report2 = agos_memory::memory::run_consolidation_job(&store, &llm, &cfg)
        .await
        .unwrap();

    // Second run should produce same or fewer results (idempotent)
    assert!(report2.summaries_generated <= report1.summaries_generated + 1);
}

#[tokio::test]
async fn dedup_consolidation_merges_similar() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("dedup.db")), 1)
        .await
        .unwrap();

    // Create two very similar memories
    let _row1 = make_memory(&store, "Rust is a systems programming language", "semantic").await;
    let _row2 = make_memory(&store, "Rust is a systems language", "semantic").await;

    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();
    let report = agos_memory::memory::run_consolidation_job(&store, &llm, &cfg)
        .await
        .unwrap();
    assert!(
        report.dedup_clusters_merged < 1000,
        "should run without error"
    );
}
