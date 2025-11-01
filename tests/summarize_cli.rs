//! Summarize CLI tests (issue 0048).

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
    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            tier: tier.into(),
            kind: "fact".into(),
            text: text.into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();
    // Set created_at far in the past so summarize_after_days threshold is met
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
    row
}

#[tokio::test]
async fn summarize_by_id_works() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("summ.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "Rust is a memory-safe systems language", "semantic").await;

    // Use the summarize_by_id function directly
    use agos_memory::llm::MockChat;
    use std::sync::Arc;
    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();

    let report = agos_memory::memory::summarize_by_id(&store, &llm, &cfg, row.id, false)
        .await
        .unwrap();
    assert!(
        !report.summary_text.is_empty(),
        "summary should not be empty"
    );
    assert!(report.summary_tokens > 0, "summary should have tokens");
}

#[tokio::test]
async fn summarize_tier_batches() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("tier_summ.db")), 1)
        .await
        .unwrap();

    make_memory(&store, "fact one", "semantic").await;
    make_memory(&store, "fact two", "semantic").await;

    use agos_memory::llm::MockChat;
    use std::sync::Arc;
    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();

    let reports = agos_memory::memory::summarize_tier(&store, &llm, &cfg, "semantic")
        .await
        .unwrap();
    assert_eq!(reports.len(), 2, "should summarize both memories");
}

#[tokio::test]
async fn summarize_force_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let store = StoreHandle::open(&test_config(&dir.path().join("force.db")), 1)
        .await
        .unwrap();

    let row = make_memory(&store, "force test", "semantic").await;

    // First summarize
    use agos_memory::llm::MockChat;
    use std::sync::Arc;
    let llm: Arc<dyn agos_memory::llm::ChatClient> = Arc::new(MockChat::default());
    let cfg = Config::default();

    let report1 = agos_memory::memory::summarize_by_id(&store, &llm, &cfg, row.id, false)
        .await
        .unwrap();
    assert!(!report1.overwrote, "first summarize should not overwrite");

    // Force overwrite
    let report2 = agos_memory::memory::summarize_by_id(&store, &llm, &cfg, row.id, true)
        .await
        .unwrap();
    assert!(report2.overwrote, "force summarize should overwrite");
}
