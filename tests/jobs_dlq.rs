//! Jobs DLQ: an always-failing handler exhausts retries, lands in
//! `jobs_dead` with status `dead`, and `requeue_dead` recovers it.

use agos_memory::config::Config;
use agos_memory::memory::worker::Worker;
use agos_memory::memory::{dead_count, enqueue, jobs};
use agos_memory::storage::StoreHandle;

async fn test_store(name: &str) -> (StoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join(name),
        ..Config::default()
    };
    let store = StoreHandle::open(&cfg, 1).await.unwrap();
    (store, dir)
}

#[tokio::test]
async fn idempotent_enqueue_returns_same_id() {
    let (store, _dir) = test_store("idem.db").await;
    let a = enqueue(&store, "extract", r#"{"s":1}"#, Some("k1"))
        .await
        .unwrap();
    let b = enqueue(&store, "extract", r#"{"s":1}"#, Some("k1"))
        .await
        .unwrap();
    assert_eq!(a, b);
    let c = enqueue(&store, "extract", r#"{"s":2}"#, Some("k2"))
        .await
        .unwrap();
    assert_ne!(a, c);
}

#[tokio::test]
async fn failing_job_reaches_dlq_and_requeues() {
    let (store, _dir) = test_store("dlq.db").await;
    // max_attempts=1 so the test runs fast: one fail → dead.
    let id = enqueue(&store, "extract", r#"{"s":9}"#, Some("dlq1"))
        .await
        .unwrap();
    store
        .write(move |conn| {
            conn.execute("UPDATE jobs SET max_attempts = 1 WHERE id = ?1", [id])?;
            Ok(())
        })
        .await
        .unwrap();

    let mut worker = Worker::new(store.clone());
    worker.on("extract", |_job, _store| async {
        Err(agos_memory::error::Error::Llm("boom".into()))
    });
    let w = std::sync::Arc::new(worker);
    let w2 = w.clone();
    let handle = tokio::spawn(async move { w2.run().await });

    // Wait for the DLQ row (poll; worker retries with backoff).
    let mut dead = 0;
    for _ in 0..100 {
        dead = dead_count(&store).await.unwrap();
        if dead == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    w.shutdown();
    handle.await.unwrap();
    assert_eq!(dead, 1, "failing job must land in jobs_dead");

    let status: String = store
        .read(move |conn| {
            conn.query_row("SELECT status FROM jobs WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|e| e.into())
        })
        .await
        .unwrap();
    assert_eq!(status, "dead");

    jobs::requeue_dead(&store, id).await.unwrap();
    let status: String = store
        .read(move |conn| {
            conn.query_row("SELECT status FROM jobs WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|e| e.into())
        })
        .await
        .unwrap();
    assert_eq!(status, "queued");
}
