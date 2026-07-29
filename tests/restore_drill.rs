//! v0.6.0 issue 0007 — executable backup/restore drill.
//!
//! The test follows the operator procedure literally: build a live database,
//! snapshot it, refuse a concurrent open, release the lock, replace the live
//! file with the snapshot, and reopen through the normal StoreHandle path.

mod common;

use std::path::{Path, PathBuf};

use agos_memory::config::{Config, RecallConfig};
use agos_memory::error::{Error, Result};
use agos_memory::memory::{persist, remember};
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use common::{DIM, NormEmbedder};

fn config(path: &Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

async fn count_where(store: &StoreHandle, sql: &str) -> i64 {
    let sql = sql.to_string();
    store
        .read(move |conn| Ok(conn.query_row(&sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

async fn remember_fact(
    store: &StoreHandle,
    emb: &NormEmbedder,
    tier: &str,
    text: &str,
) -> agos_memory::storage::MemoryRow {
    remember(
        store,
        tier,
        "fact",
        text,
        "user",
        1.0,
        emb,
        agos_memory::observe::EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn snapshot_restore_preserves_product_state_and_refuses_live_lock() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let live_path = dir.path().join("memory.db");
    let snapshot_path = dir.path().join("memory.snapshot.db");
    let moved_path = dir.path().join("memory.before-restore.db");
    let cfg = config(&live_path);
    let emb = NormEmbedder::new(DIM);

    let store = StoreHandle::open(&cfg, 2).await?;
    let active = remember_fact(
        &store,
        &emb,
        "semantic",
        "restore drill durable fact survives the snapshot",
    )
    .await;
    let episodic = remember_fact(
        &store,
        &emb,
        "episodic",
        "restore drill episodic fact survives the snapshot",
    )
    .await;
    let versioned = remember_fact(
        &store,
        &emb,
        "semantic",
        "restore versioned fact original text",
    )
    .await;
    store
        .update_memory(
            versioned.id,
            "restore versioned fact updated text",
            Some("restore update"),
            Some("restore-drill"),
        )
        .await?;
    store
        .update_memory(
            versioned.id,
            "restore versioned fact final text",
            Some("restore update"),
            Some("restore-drill"),
        )
        .await?;
    let soft_deleted = remember_fact(
        &store,
        &emb,
        "semantic",
        "restore soft deleted fact stays deprecated",
    )
    .await;
    store
        .deprecate_memory(soft_deleted.id, Some("restore-drill"))
        .await?;
    let purged = remember_fact(
        &store,
        &emb,
        "semantic",
        "restore purged bait must not return",
    )
    .await;
    store
        .hard_purge_memory(purged.id, Some("restore-drill"), Some("restore purge"))
        .await?;

    let schema_before = store.schema_version().await?;
    let dim_before = store.embed_dim().await?;
    let memories_before = store.list_memories(None, None, None).await?;
    let versions_before = store.get_memory_versions(versioned.id).await?;
    let tombstones_before = store.list_tombstones().await?;
    let audits_before = store.list_forget_audit(None, None, None).await?;
    let counts_before = store.memory_counts().await?;
    let vectors_before = count_where(&store, "SELECT count(*) FROM vec_memories").await;
    let fts_before = count_where(&store, "SELECT count(*) FROM fts_memories").await;
    let purged_id = purged.public_id.clone();
    let soft_id = soft_deleted.public_id.clone();
    let versioned_id = versioned.public_id.clone();

    let snapshot = store.snapshot_to(&snapshot_path).await?;
    assert_eq!(snapshot.integrity, "ok");
    assert_eq!(snapshot.path, snapshot_path);

    // The restore procedure's first safety gate is the same startup lock used
    // by every product command. A second open must fail before files move.
    let blocked = StoreHandle::open(&cfg, 1).await;
    assert!(
        matches!(blocked, Err(Error::DbLocked { .. })),
        "got {blocked:?}"
    );

    drop(store);
    let lock_path = PathBuf::from(format!("{}.lock", live_path.display()));
    assert!(
        lock_path.exists(),
        "a clean shutdown may leave the sidecar behind"
    );

    // Follow the runbook: preserve the old file, remove WAL/SHM sidecars from
    // the stopped database, and place the self-contained VACUUM INTO snapshot
    // at the configured live path. The existing .db.lock is intentionally left
    // in place to prove stale sidecars do not deadlock reopen.
    std::fs::rename(&live_path, &moved_path).unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", live_path.display(), suffix));
        if sidecar.exists() {
            std::fs::remove_file(sidecar).unwrap();
        }
    }
    std::fs::copy(&snapshot_path, &live_path).unwrap();

    let restored = StoreHandle::open(&cfg, 2).await?;
    assert_eq!(restored.schema_version().await?, schema_before);
    assert_eq!(restored.embed_dim().await?, dim_before);
    assert_eq!(restored.integrity().await?, "ok");
    assert_eq!(
        restored.list_memories(None, None, None).await?,
        memories_before
    );
    assert_eq!(
        restored.get_memory_versions(versioned.id).await?,
        versions_before
    );
    assert_eq!(restored.list_tombstones().await?, tombstones_before);
    assert_eq!(
        restored.list_forget_audit(None, None, None).await?,
        audits_before
    );
    assert_eq!(restored.memory_counts().await?, counts_before);
    assert_eq!(
        count_where(&restored, "SELECT count(*) FROM vec_memories").await,
        vectors_before
    );
    assert_eq!(
        count_where(&restored, "SELECT count(*) FROM fts_memories").await,
        fts_before
    );
    assert!(
        lock_path.exists(),
        "stale lock sidecar survived the restore"
    );

    assert!(restored.get_memory(purged_id.clone()).await?.is_none());
    assert_eq!(
        restored.get_memory(soft_id.clone()).await?.unwrap().status,
        "deprecated"
    );
    assert_eq!(
        restored
            .get_memory(versioned_id.clone())
            .await?
            .unwrap()
            .text,
        "restore versioned fact final text"
    );

    let mut query = RecallQuery::new("restore", &RecallConfig::default());
    query.include_episodic = true;
    query.min_score = 0.0;
    query.budget_tokens = 2_000;
    let report = recall(&restored, &emb, &query).await?;
    let ids: Vec<&str> = report
        .hits
        .iter()
        .map(|hit| hit.public_id.as_str())
        .collect();
    assert!(
        ids.contains(&active.public_id.as_str()),
        "active fact missing after restore: {ids:?}"
    );
    assert!(
        ids.contains(&episodic.public_id.as_str()),
        "episodic fact missing after restore: {ids:?}"
    );
    assert!(
        !ids.contains(&purged_id.as_str()),
        "purged fact returned after restore"
    );
    assert!(
        !ids.contains(&soft_id.as_str()),
        "soft-deleted fact returned after restore"
    );
    Ok(())
}
