//! 0055 acceptance: the `extract` job kind has a real handler.
//!
//! Before this issue the worker had no dispatch branch for `extract`, so any
//! enqueued extract job retried into the DLQ forever. Now the worker runs
//! `extract_session` on the job's session id, and the job completes.

use agos_memory::config::{Config, EmbedProvider};
use agos_memory::error::Result;
use agos_memory::llm::ChatClient;
use agos_memory::memory::jobs::{dead_count, enqueue};
use agos_memory::memory::sessions::{append_turn, close_session, open_session};
use agos_memory::memory::worker::Worker;
use agos_memory::storage::StoreHandle;
use rusqlite::params;
use std::sync::Arc;

struct FixedChat {
    response: String,
}

#[async_trait::async_trait]
impl ChatClient for FixedChat {
    async fn complete(&self, _prompt: &str) -> Result<String> {
        Ok(self.response.clone())
    }
    fn model(&self) -> &str {
        "fixed-extract"
    }
}

/// Worker config matching the test store: the worker rebuilds the embedder
/// from `config.embed` per job, so it must agree with the store's provider.
fn worker_cfg() -> Arc<Config> {
    Arc::new(Config {
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    })
}

async fn test_store() -> (StoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join("e.db"),
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    };
    let store = StoreHandle::open(&cfg, 1).await.unwrap();
    (store, dir)
}

async fn run_worker_until_done(worker: Worker, store: &StoreHandle, job_id: i64, max_ticks: usize) {
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

async fn run_worker_until_dead(worker: Worker, store: &StoreHandle, max_ticks: usize) {
    let w = Arc::new(worker);
    let w2 = w.clone();
    let handle = tokio::spawn(async move { w2.run().await });
    for _ in 0..max_ticks {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if dead_count(store).await.unwrap() == 1 {
            break;
        }
    }
    w.shutdown();
    let _ = handle.await;
}

#[tokio::test]
async fn extract_job_runs_and_completes() -> Result<()> {
    let (store, _dir) = test_store().await;
    let s = open_session(&store, "a").await?;
    append_turn(&store, s.id, "user", "I prefer tea with oat milk.").await?;
    close_session(&store, s.id, "explicit").await?;

    let chat = Arc::new(FixedChat {
        response: r#"{"memories":[{"tier":"episodic","kind":"fact","text":"user prefers tea with oat milk","importance":0.7,"confidence":0.9}]}"#
            .into(),
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg());

    let payload = serde_json::json!({"session": s.id}).to_string();
    let job_id = enqueue(&store, "extract", &payload, None).await?;

    run_worker_until_done(worker, &store, job_id, 400).await;

    assert_eq!(
        dead_count(&store).await?,
        0,
        "extract job must not land in the DLQ"
    );

    let count: i64 = store
        .read(move |conn| {
            let c: i64 = conn
                .query_row(
                    "SELECT count(*) FROM memories WHERE status = 'active'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| agos_memory::error::Error::Storage(e.to_string()))?;
            Ok(c)
        })
        .await
        .unwrap();
    assert!(
        count >= 1,
        "extract job must persist at least one memory, got {count}"
    );
    Ok(())
}

#[tokio::test]
async fn extract_job_with_bad_payload_fails_to_dlq() -> Result<()> {
    let (store, _dir) = test_store().await;
    let chat = Arc::new(FixedChat {
        response: r#"{"memories":[]}"#.into(),
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg());

    let job_id = enqueue(&store, "extract", "not json", None).await?;
    // One attempt only: the default 5 would take ~30s of backoff to reach the
    // DLQ, which is too long for a unit test.
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET max_attempts = 1 WHERE id = ?1",
                params![job_id],
            )?;
            Ok(())
        })
        .await?;
    run_worker_until_dead(worker, &store, 800).await;

    assert_eq!(
        dead_count(&store).await?,
        1,
        "malformed extract payload must fail to DLQ"
    );
    let _ = job_id;
    Ok(())
}

#[tokio::test]
async fn extract_job_is_idempotent() -> Result<()> {
    let (store, _dir) = test_store().await;
    let s = open_session(&store, "a").await?;
    append_turn(&store, s.id, "user", "I like coffee.").await?;
    close_session(&store, s.id, "explicit").await?;

    let chat = Arc::new(FixedChat {
        response: r#"{"memories":[{"tier":"episodic","kind":"fact","text":"user likes coffee","importance":0.7,"confidence":0.9}]}"#
            .into(),
    });
    let worker = Worker::new(store.clone(), chat, worker_cfg());

    let payload = serde_json::json!({"session": s.id}).to_string();
    let id1 = enqueue(&store, "extract", &payload, Some("dup-extract")).await?;
    let id2 = enqueue(&store, "extract", &payload, Some("dup-extract")).await?;
    assert_eq!(id1, id2, "enqueue must be idempotent on the key");

    run_worker_until_done(worker, &store, id1, 400).await;
    assert_eq!(dead_count(&store).await?, 0);
    Ok(())
}
