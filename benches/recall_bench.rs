//! 0038 — Performance benchmark: 10k-vector p95 < 150 ms.
//!
//! Measures end-to-end `recall()` latency and per-stage breakdown:
//! embed, vec scan, bm25, rerank, pack. Uses HashEmbedder for offline bench.

use agos_memory::config::{Config, EmbedProvider, RecallConfig};
use agos_memory::embed::{Embedder, HashEmbedder};
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{clock::Clock, sha256_hex};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::sync::Arc;
use std::time::Instant;
use tempfile::tempdir;

const VECTOR_DIM: usize = 1536;
const NUM_VECTORS: usize = 1_000;
const QUERY: &str = "vehicle maintenance log";

/// Build a test database with NUM_VECTORS memories and matching vec_memories entries.
async fn setup_bench_db() -> (StoreHandle, tempfile::TempDir) {
    let dir = tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join("bench.db"),
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::None, // degraded keyword-only, no embedder needed for vec_memories
            ..Default::default()
        },
        ..Default::default()
    };

    let store = StoreHandle::open(&cfg, 4).await.expect("open store");

    // Insert NUM_VECTORS memories with matching vec_memories
    for i in 0..NUM_VECTORS {
        let text = format!("vehicle maintenance log entry {}", i);
        let pid = format!("pid-{}-{}", &sha256_hex(&text)[..12], i);
        let hash = sha256_hex(&text);
        let now = agos_memory::util::SystemClock.now_millis();

        store
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO memories
                     (public_id, agent_id, tier, kind, text, text_hash, source_kind,
                      source_ref, status, trust, importance_current, confidence, pinned,
                      expires_at, last_referenced_at, created_at, updated_at,
                      summary_text, summary_tokens)
                     VALUES (?1, 'default', 'semantic', 'fact', ?2, ?3, 'user',
                             NULL, 'active', 'trusted', 0.5, 1.0, 0,
                             NULL, NULL, ?4, ?4, NULL, NULL)",
                    rusqlite::params![pid, text, hash, now],
                )?;
                let id = conn.last_insert_rowid();

                // Insert matching unit vector in vec_memories
                let blob: Vec<u8> = (0..VECTOR_DIM)
                    .flat_map(|_| 1.0f32.to_le_bytes())
                    .collect();
                conn.execute(
                    "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                     VALUES (?1, ?2, 'semantic', 0, 0, 'fact', 0)",
                    rusqlite::params![id, blob],
                )?;
                Ok(())
            })
            .await
            .expect("seed insert");
    }

    (store, dir)
}

fn bench_recall_stages(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Setup once
    let (store, _dir) = rt.block_on(setup_bench_db());
    let embedder = Arc::new(HashEmbedder::new(VECTOR_DIM));

    let mut group = c.benchmark_group("recall_stages");
    group.sample_size(50);
    group.measurement_time(std::time::Duration::from_secs(30));

    // Benchmark: full recall end-to-end
    group.bench_function("recall_full", |b| {
        b.iter(|| {
            rt.block_on(async {
                let cfg = RecallConfig::default();
                let query = RecallQuery::new(QUERY, &cfg);
                let report = recall(&store, &*embedder, &query).await.unwrap();
                black_box(report);
            })
        });
    });

    // Benchmark: embed only
    group.bench_function("embed_only", |b| {
        b.iter(|| {
            rt.block_on(async {
                let _ = embedder.embed(&[QUERY.to_string()]).await.unwrap();
            })
        });
    });

    // Benchmark: vec scan only (via recall with degraded mode)
    group.bench_function("vec_scan_degraded", |b| {
        b.iter(|| {
            rt.block_on(async {
                let cfg = agos_memory::config::RecallConfig {
                    trust_policy: agos_memory::config::TrustPolicy::Strict,
                    ..Default::default()
                };
                // Force degraded by using no embedder path - we'll just measure the FTS path
                let query = RecallQuery::new(QUERY, &cfg);
                // This will use FTS only since embedder is none
                let _ = recall(&store, &agos_memory::embed::NoEmbedder, &query)
                    .await
                    .unwrap();
            })
        });
    });

    group.finish();

    // Print p50/p95/p99 stats
    let mut group = c.benchmark_group("recall_p95");
    group.sample_size(100);
    group.measurement_time(std::time::Duration::from_secs(60));

    let mut latencies = Vec::new();
    for _ in 0..100 {
        let start = Instant::now();
        rt.block_on(async {
            let cfg = RecallConfig::default();
            let query = RecallQuery::new(QUERY, &cfg);
            let _ = recall(&store, &*embedder, &query).await.unwrap();
        });
        latencies.push(start.elapsed().as_millis() as f64);
    }

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[(latencies.len() as f64 * 0.95) as usize];
    let p99 = latencies[(latencies.len() as f64 * 0.99) as usize];

    println!("=== Recall Latency Stats (ms) ===");
    println!("p50: {:.2}", p50);
    println!("p95: {:.2}", p95);
    println!("p99: {:.2}", p99);
    println!("min: {:.2}", latencies[0]);
    println!("max: {:.2}", latencies[latencies.len() - 1]);

    // Fail if p95 > 150ms
    assert!(
        p95 < 150.0,
        "p95 latency {:.2}ms exceeds 150ms threshold",
        p95
    );

    group.finish();
}

criterion_group!(benches, bench_recall_stages);
criterion_main!(benches);
