//! 0055 acceptance: the `maintain` job kind folds consolidation in via payload
//! (D35), so there is no `consolidate` job kind and no schema CHECK change.

use agos_memory::config::{Config, EmbedProvider};
use agos_memory::error::Result;
use agos_memory::llm::ChatClient;
use agos_memory::memory::jobs::{dead_count, enqueue};
use rusqlite::params;
use agos_memory::memory::worker::Worker;
use agos_memory::storage::StoreHandle;
use std::sync::Arc;

struct EmptyChat;
#[async_trait::async_trait]
impl ChatClient for EmptyChat {
    async fn complete(&self, _p: &str) -> Result<String> {
        Ok(r#"{"memories":[]}"#.into())
    }
    fn model(&self) -> &str {
        "empty"
    }
}

async fn test_store() -> (StoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join("m.db"),
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    };
    (StoreHandle::open(&cfg, 1).await.unwrap(), dir)
}

async fn run_worker(worker: Worker, store: &StoreHandle, job_id: i64, max_ticks: usize) {
    let w = Arc::new(worker);
    let w2 = w.clone();
    let handle = tokio::spawn(async move { w2.run().await });
    for _ in 0..max_ticks {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let done = store
            .read(move |conn| {
                let c: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM jobs WHERE id = ?1 AND status = 'done'",
                        [job_id],
                        |r| r.get(0),
                    )
                    .map_err(|e| agos_memory::error::Error::Storage(e.to_string()))?;
                Ok(c)
            })
            .await
            .unwrap();
        if done == 1 {
            break;
        }
    }
    w.shutdown();
    let _ = handle.await;
}

#[tokio::test]
async fn maintain_consolidate_payload_runs() -> Result<()> {
    let (store, _dir) = test_store().await;
    let chat = Arc::new(EmptyChat);
    let worker_cfg = Arc::new(Config {
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg);

    // D35: consolidation is a `maintain` job with `{"action":"consolidate"}`.
    let payload = serde_json::json!({"action": "consolidate"}).to_string();
    let job_id = enqueue(&store, "maintain", &payload, Some("consolidate-1")).await?;

    run_worker(worker, &store, job_id, 400).await;
    assert_eq!(
        dead_count(&store).await?,
        0,
        "consolidate job must not go to DLQ"
    );
    Ok(())
}

#[tokio::test]
async fn maintain_ttl_payload_runs() -> Result<()> {
    let (store, _dir) = test_store().await;
    let chat = Arc::new(EmptyChat);
    let worker_cfg = Arc::new(Config {
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg);

    let payload = serde_json::json!({"action": "ttl"}).to_string();
    let job_id = enqueue(&store, "maintain", &payload, Some("ttl-1")).await?;

    run_worker(worker, &store, job_id, 400).await;
    assert_eq!(dead_count(&store).await?, 0, "ttl job must not go to DLQ");
    Ok(())
}

#[tokio::test]
async fn maintain_unknown_action_fails_to_dlq() -> Result<()> {
    let (store, _dir) = test_store().await;
    let chat = Arc::new(EmptyChat);
    let worker_cfg = Arc::new(Config {
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg);

    let payload = serde_json::json!({"action": "bogus"}).to_string();
    let job_id = enqueue(&store, "maintain", &payload, Some("bogus-1")).await?;
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET max_attempts = 1 WHERE id = ?1",
                params![job_id],
            )?;
            Ok(())
        })
        .await?;

    let w = Arc::new(worker);
    let w2 = w.clone();
    let handle = tokio::spawn(async move { w2.run().await });
    for _ in 0..800 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if dead_count(&store).await.unwrap() == 1 {
            break;
        }
    }
    w.shutdown();
    let _ = handle.await;

    assert_eq!(
        dead_count(&store).await?,
        1,
        "unknown maintain action must fail to DLQ"
    );
    Ok(())
}

#[tokio::test]
async fn maintain_payload_is_idempotent() -> Result<()> {
    let (store, _dir) = test_store().await;
    let chat = Arc::new(EmptyChat);
    let worker_cfg = Arc::new(Config {
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg);

    let payload = serde_json::json!({"action": "consolidate"}).to_string();
    let id1 = enqueue(&store, "maintain", &payload, Some("consolidate-idem")).await?;
    let id2 = enqueue(&store, "maintain", &payload, Some("consolidate-idem")).await?;
    assert_eq!(id1, id2, "enqueue must be idempotent on the key");

    run_worker(worker, &store, id1, 400).await;
    assert_eq!(dead_count(&store).await?, 0);
    Ok(())
}
