//! v0.6.0 sustained mixed-traffic soak test.
//!
//! This test is intentionally ignored by ordinary `cargo test` because it is a
//! release/operations gate. Run it with `make soak`; set `AGOS_SOAK_SECS` for a
//! longer run or a short diagnostic run.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agos_memory::config::{Config, EmbedProvider};
use agos_memory::embed::HashEmbedder;
use agos_memory::error::{Error, Result};
use agos_memory::memory::extract::Candidate;
use agos_memory::memory::persist::{DEDUP_THRESHOLD, persist_candidate_full};
use agos_memory::memory::{jobs, sessions, worker::Worker};
use agos_memory::observe::EXTRACTOR_VERSION;
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use tokio::sync::Mutex;

const OP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_SECS: u64 = 60;
const MAX_SECS: u64 = 600;

struct FixedChat;

#[async_trait::async_trait]
impl agos_memory::llm::ChatClient for FixedChat {
    async fn complete(&self, _prompt: &str) -> Result<String> {
        Ok(r#"{"memories":[]}"#.into())
    }

    fn model(&self) -> &str {
        "soak-fixed"
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
}

fn sidecar_bytes(path: &Path, suffix: &str) -> u64 {
    file_bytes(Path::new(&format!("{}{}", path.display(), suffix)))
}

/// Linux resident-set size in KiB; None on platforms without `/proc/self/statm`.
fn rss_kib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        Some(resident_pages.saturating_mul(4096 / 1024))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

async fn timed<T, F>(future: F, operation: &'static str) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    tokio::time::timeout(OP_TIMEOUT, future)
        .await
        .map_err(|_| Error::Storage(format!("{operation} timed out after {OP_TIMEOUT:?}")))?
}

async fn direct_count(store: &StoreHandle) -> Result<i64> {
    store
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM memories WHERE text LIKE 'soak-remember-%'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
}

async fn table_counts(store: &StoreHandle) -> Result<Vec<(String, i64)>> {
    store
        .read(|conn| {
            let mut out = Vec::new();
            for table in [
                "memories",
                "sessions",
                "turns",
                "jobs",
                "jobs_dead",
                "tombstones",
                "forget_audit",
            ] {
                out.push((
                    table.to_string(),
                    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?,
                ));
            }
            Ok(out)
        })
        .await
}

#[tokio::test]
#[ignore = "run with make soak"]
async fn sustained_mixed_traffic_has_no_deadlock_leak_or_unbounded_growth() -> Result<()> {
    let seconds = env_u64("AGOS_SOAK_SECS", DEFAULT_SECS).clamp(1, MAX_SECS);
    let max_rss_growth_kib = env_u64("AGOS_SOAK_MAX_RSS_GROWTH_KIB", 128 * 1024);
    let max_db_growth = env_u64("AGOS_SOAK_MAX_DB_GROWTH_BYTES", 64 * 1024 * 1024);
    let max_wal_bytes = env_u64("AGOS_SOAK_MAX_WAL_BYTES", 32 * 1024 * 1024);
    let duration = Duration::from_secs(seconds);

    // Warm the writer, sqlite-vec, and async runtime before establishing the
    // RSS baseline. Otherwise one-time initialization is mistaken for a leak.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let deadline = Instant::now() + duration;

    let dir = tempfile::tempdir().map_err(|e| Error::Storage(e.to_string()))?;
    let db_path = dir.path().join("soak.db");
    let mut cfg = Config {
        db_path: db_path.clone(),
        agent_id: "soak-agent".into(),
        ..Config::default()
    };
    cfg.embed.provider = EmbedProvider::Hash;
    cfg.recall.top_k = 8;
    cfg.recall.budget_tokens = 1500;
    let store = StoreHandle::open(&cfg, 4).await?;
    let dim = store.embed_dim().await?;
    let embedder = Arc::new(HashEmbedder::new(dim));
    store.validate_embed_dim(embedder.as_ref()).await?;

    let worker = Arc::new(Worker::new(
        store.clone(),
        Arc::new(FixedChat),
        Arc::new(cfg.clone()),
    ));
    let worker_task = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.run().await })
    };

    let expected_rows = Arc::new(Mutex::new(HashMap::<String, Option<String>>::new()));
    let stop_sampling = Arc::new(AtomicBool::new(false));
    let peak_wal = Arc::new(AtomicU64::new(0));
    let peak_rss = Arc::new(AtomicU64::new(0));
    let ops = Arc::new(AtomicU64::new(0));
    let remember_ops = Arc::new(AtomicU64::new(0));
    let recall_ops = Arc::new(AtomicU64::new(0));
    let hard_purges = Arc::new(AtomicU64::new(0));
    let session_count = Arc::new(AtomicU64::new(0));
    let maintain_count = Arc::new(AtomicU64::new(0));

    let retain_bytes_per_op = env_u64("AGOS_SOAK_RETAIN_BYTES_PER_OP", 0);
    let retained_buffers = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let rss_start = rss_kib();
    let db_start = file_bytes(&db_path);
    println!(
        "soak: start secs={seconds} rss_kib={rss_start:?} db_bytes={db_start} \
         writer_healthy={}",
        store.writer_is_healthy()
    );

    let sampler_stop = stop_sampling.clone();
    let sampler_store = store.clone();
    let sampler_path = db_path.clone();
    let sampler_peak_wal = peak_wal.clone();
    let sampler_peak_rss = peak_rss.clone();
    let sampler = tokio::spawn(async move {
        let mut sample = 0u64;
        while !sampler_stop.load(Ordering::Relaxed) {
            let wal = sidecar_bytes(&sampler_path, "-wal");
            sampler_peak_wal.fetch_max(wal, Ordering::Relaxed);
            if let Some(rss) = rss_kib() {
                sampler_peak_rss.fetch_max(rss, Ordering::Relaxed);
            }
            // Checkpoint periodically; the peak is sampled before the
            // checkpoint so the bound proves checkpointing keeps WAL finite.
            if sample.is_multiple_of(10) {
                let _ = timed(sampler_store.wal_checkpoint(), "wal-checkpoint").await;
            }
            sample += 1;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let writer_store = store.clone();
    let writer_embedder = embedder.clone();
    let writer_cfg = cfg.clone();
    let writer_expected = expected_rows.clone();
    let writer_ops = ops.clone();
    let writer_remember_ops = remember_ops.clone();
    let writer_hard = hard_purges.clone();
    let writer_leak_bytes = retain_bytes_per_op;
    let writer_leak_buffers = retained_buffers.clone();
    let writer_lane = tokio::spawn(async move {
        let mut i = 0u64;
        while Instant::now() < deadline {
            let text = format!("soak-remember-{i} unique retained fact");
            let candidate = Candidate {
                tier: ["working", "episodic", "semantic", "procedural"][(i % 4) as usize].into(),
                kind: if i.is_multiple_of(2) { "fact" } else { "note" }.into(),
                text: text.clone(),
                importance: 0.5,
                confidence: 1.0,
                session_independent: true,
                source_seq: i as i64,
            };
            let row = timed(
                persist_candidate_full(
                    &writer_store,
                    &candidate,
                    writer_embedder.as_ref(),
                    EXTRACTOR_VERSION,
                    "user",
                    writer_cfg.memory.pending_threshold,
                    DEDUP_THRESHOLD,
                ),
                "remember",
            )
            .await?
            .row;
            writer_expected
                .lock()
                .await
                .insert(row.public_id.clone(), Some(row.status.clone()));
            writer_remember_ops.fetch_add(1, Ordering::Relaxed);
            writer_ops.fetch_add(1, Ordering::Relaxed);
            if writer_leak_bytes > 0 {
                writer_leak_buffers
                    .lock()
                    .await
                    .push(vec![0u8; writer_leak_bytes as usize]);
            }

            if i.is_multiple_of(17) {
                timed(
                    writer_store.hard_purge_memory(row.id, Some("soak"), Some("soak purge")),
                    "hard-forget",
                )
                .await?;
                writer_hard.fetch_add(1, Ordering::Relaxed);
                let mut rows = writer_expected.lock().await;
                if let Some(entry) = rows.get_mut(&row.public_id) {
                    *entry = None;
                }
            } else if i.is_multiple_of(3) {
                timed(
                    writer_store.deprecate_memory(row.id, Some("soak")),
                    "soft-forget",
                )
                .await?;
                let mut rows = writer_expected.lock().await;
                if let Some(entry) = rows.get_mut(&row.public_id) {
                    *entry = Some("deprecated".into());
                }
            }
            i += 1;
            tokio::task::yield_now().await;
        }
        Ok::<(), Error>(())
    });

    let recall_store = store.clone();
    let recall_embedder = embedder.clone();
    let recall_cfg = cfg.clone();
    let recall_ops = recall_ops.clone();
    let all_ops = ops.clone();
    let recall_lane = tokio::spawn(async move {
        let queries = [
            "soak remember unique retained fact",
            "deployment database migration",
            "user preference and project notes",
            "nothing relevant in this query",
        ];
        let mut i = 0usize;
        while Instant::now() < deadline {
            let mut query = RecallQuery::new(queries[i % queries.len()], &recall_cfg.recall);
            query.k = 8;
            let report = timed(
                recall(&recall_store, recall_embedder.as_ref(), &query),
                "recall",
            )
            .await?;
            let _ = report;
            recall_ops.fetch_add(1, Ordering::Relaxed);
            all_ops.fetch_add(1, Ordering::Relaxed);
            i += 1;
            tokio::task::yield_now().await;
        }
        Ok::<(), Error>(())
    });

    let session_store = store.clone();
    let sessions_done = session_count.clone();
    let all_ops = ops.clone();
    let session_lane = tokio::spawn(async move {
        let mut i = 0u64;
        while Instant::now() < deadline {
            let session = timed(
                sessions::open_session(&session_store, "soak-agent"),
                "session-open",
            )
            .await?;
            timed(
                sessions::append_turn(
                    &session_store,
                    session.id,
                    "user",
                    &format!("soak session turn {i}: remember this note"),
                ),
                "session-append",
            )
            .await?;
            timed(
                sessions::close_session(&session_store, session.id, "explicit"),
                "session-close",
            )
            .await?;
            let payload = serde_json::json!({"session": session.id}).to_string();
            let key = format!("soak-extract-{i}");
            timed(
                jobs::enqueue(&session_store, "extract", &payload, Some(&key)),
                "enqueue-extract",
            )
            .await?;
            sessions_done.fetch_add(1, Ordering::Relaxed);
            all_ops.fetch_add(4, Ordering::Relaxed);
            i += 1;
            // Session extraction is asynchronous. Pace this producer below the
            // single worker's service rate so the bounded run drains cleanly.
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        Ok::<(), Error>(())
    });

    let maintain_store = store.clone();
    let maintain_done = maintain_count.clone();
    let all_ops = ops.clone();
    let maintain_lane = tokio::spawn(async move {
        let mut i = 0u64;
        while Instant::now() < deadline {
            let action = if i.is_multiple_of(4) { "all" } else { "ttl" };
            let payload = serde_json::json!({"action": action}).to_string();
            let key = format!("soak-maintain-ttl-{i}");
            timed(
                jobs::enqueue(&maintain_store, "maintain", &payload, Some(&key)),
                "enqueue-maintain",
            )
            .await?;
            maintain_done.fetch_add(1, Ordering::Relaxed);
            all_ops.fetch_add(1, Ordering::Relaxed);
            i += 1;
            // Full maintenance is intentionally serialized behind the worker;
            // keep the producer comfortably below its service rate.
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
        Ok::<(), Error>(())
    });

    stop_sampling.store(true, Ordering::Relaxed);
    let (writer_result, recall_result, session_result, maintain_result) =
        tokio::join!(writer_lane, recall_lane, session_lane, maintain_lane);
    writer_result.map_err(|e| Error::Storage(format!("writer lane join failed: {e}")))??;
    recall_result.map_err(|e| Error::Storage(format!("recall lane join failed: {e}")))??;
    session_result.map_err(|e| Error::Storage(format!("session lane join failed: {e}")))??;
    maintain_result.map_err(|e| Error::Storage(format!("maintain lane join failed: {e}")))??;
    let _ = sampler.await;

    // Let the worker drain every queued job before stopping it.
    let drain_deadline = Instant::now() + Duration::from_secs(30);
    let mut pending_jobs = 0i64;
    while Instant::now() < drain_deadline {
        pending_jobs = store
            .read(|conn| {
                Ok(conn.query_row(
                    "SELECT count(*) FROM jobs WHERE status IN ('queued','running')",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await?;
        if pending_jobs == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let writer_healthy_before_shutdown = store.writer_is_healthy();
    worker.shutdown();
    let _ = worker_task.await;

    assert_eq!(pending_jobs, 0, "worker left jobs queued or running");
    assert!(
        writer_healthy_before_shutdown,
        "single writer became unhealthy"
    );

    let rss_end = rss_kib();
    let db_end = file_bytes(&db_path);
    let wal_end = sidecar_bytes(&db_path, "-wal");
    let peak_wal_bytes = peak_wal.load(Ordering::Relaxed).max(wal_end);
    let peak_rss_kib = peak_rss.load(Ordering::Relaxed).max(rss_end.unwrap_or(0));
    let direct_rows = direct_count(&store).await?;
    let expected_rows = expected_rows.lock().await.clone();
    let expected_retained = expected_rows
        .values()
        .filter(|status| status.is_some())
        .count() as i64;
    let counts = table_counts(&store).await?;
    let dead_jobs: i64 = counts
        .iter()
        .find(|(table, _)| table == "jobs_dead")
        .map(|(_, count)| *count)
        .unwrap_or(0);
    let tombstones: i64 = counts
        .iter()
        .find(|(table, _)| table == "tombstones")
        .map(|(_, count)| *count)
        .unwrap_or(0);
    let done_jobs: i64 = store
        .read(|conn| {
            Ok(
                conn.query_row("SELECT count(*) FROM jobs WHERE status = 'done'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await?;
    let expected_jobs = session_count.load(Ordering::Relaxed) as i64
        + maintain_count.load(Ordering::Relaxed) as i64;

    println!(
        "soak: end ops={} ops_per_sec={:.2} rss_start_kib={rss_start:?} \
         rss_peak_kib={peak_rss_kib} rss_end_kib={rss_end:?} db_start_bytes={db_start} \
         db_end_bytes={db_end} wal_peak_bytes={peak_wal_bytes} wal_end_bytes={wal_end}",
        ops.load(Ordering::Relaxed),
        ops.load(Ordering::Relaxed) as f64 / duration.as_secs_f64(),
    );
    println!(
        "soak: reconciliation remember={} direct_rows={direct_rows} expected_retained={expected_retained} \
         hard_purges={} sessions={} turns={} jobs_done={done_jobs} jobs_expected={expected_jobs} \
         jobs_dead={dead_jobs} tombstones={tombstones}",
        remember_ops.load(Ordering::Relaxed),
        hard_purges.load(Ordering::Relaxed),
        counts
            .iter()
            .find(|(table, _)| table == "sessions")
            .map(|(_, count)| count)
            .unwrap_or(&0),
        counts
            .iter()
            .find(|(table, _)| table == "turns")
            .map(|(_, count)| count)
            .unwrap_or(&0),
    );
    println!("soak: table_counts={counts:?}");

    assert_eq!(
        direct_rows, expected_retained,
        "remembered rows do not reconcile"
    );
    assert_eq!(dead_jobs, 0, "no soak job may enter the DLQ");
    assert_eq!(tombstones, hard_purges.load(Ordering::Relaxed) as i64);
    assert_eq!(
        done_jobs, expected_jobs,
        "queued maintenance work was not drained"
    );
    if let (Some(start), Some(end)) = (rss_start, rss_end) {
        assert!(
            end <= start.saturating_add(max_rss_growth_kib),
            "RSS grew from {start} KiB to {end} KiB (limit +{max_rss_growth_kib} KiB)"
        );
    } else {
        println!("soak: RSS assertion skipped: /proc/self/statm is unavailable on this platform");
    }
    assert!(
        peak_wal_bytes <= max_wal_bytes,
        "WAL peaked at {peak_wal_bytes} bytes (limit {max_wal_bytes})"
    );
    assert!(
        db_end <= db_start.saturating_add(max_db_growth),
        "database grew from {db_start} to {db_end} bytes (limit +{max_db_growth})"
    );

    for (public_id, expected_status) in expected_rows {
        let actual = store.get_memory(public_id.clone()).await?;
        match expected_status {
            Some(expected_status) => assert_eq!(
                actual.map(|row| row.status),
                Some(expected_status),
                "retained row {public_id} changed status unexpectedly"
            ),
            None => assert!(actual.is_none(), "hard-purged row {public_id} still exists"),
        }
    }
    Ok(())
}
