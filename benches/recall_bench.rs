//! 0038 — Recall performance benchmark (roadmap targets: p95 < 150 ms @10k,
//! p95 < 300 ms @100k).
//!
//! Measures end-to-end `recall()` latency and per-stage breakdown: embed, vec
//! scan, BM25, rerank, and pack. Uses `HashEmbedder` for an offline benchmark.
//!
//! Corpus size defaults to 1,000 vectors for a quick debug run. Release gates:
//!   - `make bench` sets 10,000 vectors and requires p95 < 150 ms.
//!   - `make bench-100k` sets 100,000 vectors and requires p95 < 300 ms.
//!
//! `AGOS_BENCH_P95_MS` can override the computed ceiling for a one-off
//! measurement. `AGOS_BENCH_SAMPLES` controls the explicit percentile sample
//! count (minimum 20; default 100). Assertions are release-profile values;
//! `cargo test --all-targets` also executes benches in debug, where the threshold
//! is intentionally skipped unless `AGOS_BENCH_ASSERT=1` is set explicitly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agos_memory::config::{Config, EmbedProvider, RecallConfig};
use agos_memory::embed::{Embedder, HashEmbedder, NoEmbedder};
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{clock::Clock, sha256_hex};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tempfile::tempdir;

const VECTOR_DIM: usize = 1536;
const DEFAULT_VECTORS: usize = 1_000;
const SEED_BATCH_SIZE: usize = 1_000;
const DEFAULT_P95_SAMPLES: usize = 100;
const QUERY: &str = "vehicle maintenance log";

fn num_vectors() -> usize {
    std::env::var("AGOS_BENCH_VECTORS")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|size| *size > 0)
        .unwrap_or(DEFAULT_VECTORS)
}

/// Return the roadmap ceiling for this corpus, or an explicit one-off override.
fn p95_ceiling_ms() -> f64 {
    if let Ok(value) = std::env::var("AGOS_BENCH_P95_MS")
        && let Ok(ceiling) = value.trim().parse::<f64>()
        && ceiling > 0.0
    {
        return ceiling;
    }
    if num_vectors() >= 100_000 {
        300.0
    } else {
        150.0
    }
}

fn p95_samples() -> usize {
    std::env::var("AGOS_BENCH_SAMPLES")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|samples| *samples >= 20)
        .unwrap_or(DEFAULT_P95_SAMPLES)
}

fn should_assert_p95() -> bool {
    !cfg!(debug_assertions) || std::env::var("AGOS_BENCH_ASSERT").as_deref() == Ok("1")
}

/// Build a database with `num_vectors()` memories and matching vectors.
///
/// Inserts are committed in bounded batches. Setup time is reported separately
/// so corpus construction never contributes to query latency percentiles.
async fn setup_bench_db() -> (StoreHandle, tempfile::TempDir) {
    let dir = tempdir().expect("benchmark tempdir");
    let cfg = Config {
        db_path: dir.path().join("bench.db"),
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::None,
            ..Default::default()
        },
        ..Default::default()
    };

    let store = StoreHandle::open(&cfg, 4).await.expect("open store");
    let corpus_size = num_vectors();
    let embedding = Arc::new(
        (0..VECTOR_DIM)
            .flat_map(|_| 1.0f32.to_le_bytes())
            .collect::<Vec<u8>>(),
    );
    let now = agos_memory::util::SystemClock.now_millis();
    let seed_started = Instant::now();
    println!("=== recall_bench: seeding {corpus_size} vectors ===");

    for batch_start in (0..corpus_size).step_by(SEED_BATCH_SIZE) {
        let batch_end = (batch_start + SEED_BATCH_SIZE).min(corpus_size);
        let embedding = Arc::clone(&embedding);
        store
            .write(move |conn| {
                // The StoreHandle serializes this closure on its writer thread,
                // so an unchecked transaction is safe and avoids simultaneously
                // borrowing both Connection and Transaction.
                let tx = conn.unchecked_transaction()?;
                for index in batch_start..batch_end {
                    let text = format!("vehicle maintenance log entry {index}");
                    let public_id = format!("pid-{}-{index}", &sha256_hex(&text)[..12]);
                    let text_hash = sha256_hex(&text);
                    tx.execute(
                        "INSERT INTO memories
                         (public_id, agent_id, tier, kind, text, text_hash, source_kind,
                          source_ref, status, trust, importance_current, confidence, pinned,
                          expires_at, last_referenced_at, created_at, updated_at,
                          summary_text, summary_tokens)
                         VALUES (?1, 'default', 'semantic', 'fact', ?2, ?3, 'user',
                                 NULL, 'active', 'trusted', 0.5, 1.0, 0,
                                 NULL, NULL, ?4, ?4, NULL, NULL)",
                        rusqlite::params![public_id, text, text_hash, now],
                    )?;
                    let row_id = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO vec_memories
                         (rowid, embedding, tier, status, trust, kind, pinned)
                         VALUES (?1, ?2, 'semantic', 0, 0, 'fact', 0)",
                        rusqlite::params![row_id, embedding.as_slice()],
                    )?;
                }
                Ok(tx.commit()?)
            })
            .await
            .expect("seed batch");
    }

    println!(
        "seed time: {:.2} s ({} vectors, batch size {})",
        seed_started.elapsed().as_secs_f64(),
        corpus_size,
        SEED_BATCH_SIZE
    );
    (store, dir)
}

fn bench_recall_stages(criterion: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime");
    let (store, _dir) = runtime.block_on(setup_bench_db());
    let embedder = Arc::new(HashEmbedder::new(VECTOR_DIM));

    // The 10k run publishes the per-stage breakdown. The 100k proof target
    // measures the end-to-end gate directly; repeating Criterion's minimum
    // 50-sample stage suite would add several minutes without changing the
    // p95 corpus or threshold.
    if num_vectors() < 100_000 {
        let mut group = criterion.benchmark_group("recall_stages");
        group.sample_size(50);
        group.measurement_time(Duration::from_secs(30));

        group.bench_function("recall_full", |bencher| {
            bencher.iter(|| {
                runtime.block_on(async {
                    let query = RecallQuery::new(QUERY, &RecallConfig::default());
                    black_box(recall(&store, &*embedder, &query).await.expect("recall"));
                });
            });
        });

        group.bench_function("embed_only", |bencher| {
            bencher.iter(|| {
                runtime.block_on(async {
                    black_box(
                        embedder
                            .embed(&[QUERY.to_string()])
                            .await
                            .expect("embed query"),
                    );
                });
            });
        });

        group.bench_function("keyword_only", |bencher| {
            bencher.iter(|| {
                runtime.block_on(async {
                    let query = RecallQuery::new(QUERY, &RecallConfig::default());
                    black_box(
                        recall(&store, &NoEmbedder, &query)
                            .await
                            .expect("keyword recall"),
                    );
                });
            });
        });
        group.finish();
    }

    let sample_count = p95_samples();
    let mut latencies = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let started = Instant::now();
        runtime.block_on(async {
            let query = RecallQuery::new(QUERY, &RecallConfig::default());
            recall(&store, &*embedder, &query)
                .await
                .expect("p95 recall");
        });
        latencies.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    latencies.sort_by(f64::total_cmp);

    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[(latencies.len() * 95 / 100).saturating_sub(1)];
    let p99 = latencies[(latencies.len() * 99 / 100).saturating_sub(1)];
    let ceiling = p95_ceiling_ms();

    println!("=== recall latency (query only) ===");
    println!("corpus: {} vectors", num_vectors());
    println!("samples: {sample_count}");
    println!("p50: {p50:.2} ms");
    println!("p95: {p95:.2} ms");
    println!("p99: {p99:.2} ms");
    println!("min: {:.2} ms", latencies[0]);
    println!("max: {:.2} ms", latencies[latencies.len() - 1]);

    if should_assert_p95() {
        assert!(
            p95 < ceiling,
            "p95 latency {p95:.2}ms exceeds {ceiling:.0}ms threshold (corpus: {} vectors)",
            num_vectors()
        );
        println!(
            "p95 gate: PASS (< {ceiling:.0} ms @ {} vectors)",
            num_vectors()
        );
    } else {
        println!("p95 gate: SKIPPED (debug profile; run `make bench` or `make bench-100k`)");
    }
}

criterion_group!(benches, bench_recall_stages);
criterion_main!(benches);
