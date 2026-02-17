//! Consolidation benchmark suite (issue 0049).
//!
//! Criterion benchmarks for the v0.4.0 consolidation path, hermetic and
//! offline (`EmbedProvider::None` + `MockChat`):
//!
//! - `summarize_quality` — single-memory summarize + ROUGE-L quality score
//! - `summarize_speed` — single-memory summarize latency
//! - `batch_summarize_throughput` — full periodic summarization pass
//! - `dedup_consolidation_speed` — cosine dedup + orphan cleanup
//! - `ttl_reaper_throughput` — expiry scan + soft-deprecate loop
//!
//! Quality/throughput assertions apply only to release runs (`cargo bench`)
//! or with `AGOS_BENCH_ASSERT=1`, mirroring `benches/recall_bench.rs`. The
//! ≥ 0.85 ROUGE-L acceptance gate itself lives in
//! `tests/summarize_quality.rs` against `fixtures/summarize_cases.jsonl`.

use std::sync::Arc;
use std::time::Instant;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use tokio::runtime::Runtime;

use agos_memory::config::{Config, EmbedConfig, EmbedProvider};
use agos_memory::llm::{ChatClient, MockChat};
use agos_memory::storage::StoreHandle;
use agos_memory::util::Clock;

/// Whether quality assertions apply to this profile (release benches and
/// explicit opt-in runs only; debug `cargo test --all-targets` skips them).
fn should_assert() -> bool {
    !cfg!(debug_assertions) || std::env::var("AGOS_BENCH_ASSERT").as_deref() == Ok("1")
}

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        embed: EmbedConfig {
            provider: EmbedProvider::None,
            ..Default::default()
        },
        memory: agos_memory::config::MemoryConfig {
            // Short working TTL so the reaper bench has expired rows.
            ttl_working_days: 3,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Insert one memory and backdate it so summarization eligibility
/// (`summarize_after_days_*`) is always met.
async fn insert_old_memory(store: &StoreHandle, text: &str, tier: &str) -> i64 {
    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            text: text.into(),
            tier: tier.into(),
            kind: "fact".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET created_at = 1 WHERE id = ?1",
                rusqlite::params![row.id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    row.id
}

/// Insert `n` memories with `created_at` 31 days in the past (TTL bait).
async fn insert_ttl_memories(store: &StoreHandle, now_millis: i64, n: usize) {
    let created = now_millis - 31 * 24 * 60 * 60 * 1000;
    store
        .write(move |conn| {
            for i in 0..n {
                let text = format!("TTL test memory number {} about the system", i + 1);
                let hash = agos_memory::util::sha256_hex(&text);
                conn.execute(
                    "INSERT INTO memories
                     (public_id, agent_id, tier, kind, text, text_hash, source_kind,
                      source_ref, status, trust, importance_current, confidence, pinned,
                      expires_at, last_referenced_at, created_at, updated_at,
                      summary_text, summary_tokens)
                     VALUES (?1, 'default', ?2, 'fact', ?3, ?4, 'user',
                             NULL, 'active', 'trusted', 0.5, 1.0, 0,
                             NULL, NULL, ?5, ?5, NULL, NULL)",
                    rusqlite::params![format!("ttl-bench-{}", i), "working", text, hash, created],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// Clear every summary so the batch pass has work again (idempotence would
/// otherwise make later iterations measure a no-op).
async fn clear_summaries(store: &StoreHandle) {
    store
        .write(|conn| {
            conn.execute(
                "UPDATE memories SET summary_text = NULL, summary_tokens = 0",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Make every memory active again so the reaper has work each iteration.
async fn reset_ttl_status(store: &StoreHandle) {
    store
        .write(|conn| {
            conn.execute(
                "UPDATE memories SET status = 'active', deleted_at = NULL
                 WHERE public_id LIKE 'ttl-bench-%'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Reaper pass: scan expired rows, then soft-deprecate each (the same calls
/// `src/memory/ttl_reaper.rs` makes, minus the grace-purge phase).
async fn ttl_reaper_pass(store: &StoreHandle, cfg: &Config) -> usize {
    let now = agos_memory::util::SystemClock.now_millis();
    let expired = store
        .expired_memories(
            now,
            cfg.memory.ttl_working_days as i64,
            cfg.memory.ttl_episodic_days as i64,
            cfg.memory.ttl_semantic_days as i64,
            cfg.memory.ttl_procedural_days as i64,
        )
        .await
        .unwrap();
    let count = expired.len();
    for (id, _, _) in expired {
        store.deprecate_for_ttl(id).await.unwrap();
    }
    count
}

fn bench_summarize_quality(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("quality.db"));
    let rt = Runtime::new().unwrap();
    let store = rt.block_on(async { StoreHandle::open(&cfg, 2).await.unwrap() });
    let text = "Rust is a systems programming language focused on safety and performance.";
    let id = rt.block_on(async { insert_old_memory(&store, text, "semantic").await });

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed(
        "Rust systems programming language, focused on safety and performance.",
    ));

    c.bench_function("summarize_quality", |b| {
        b.to_async(&rt).iter(|| {
            let chat = chat.clone();
            let store = store.clone();
            let cfg = cfg.clone();
            async move {
                let report = agos_memory::memory::summarize_by_id(&store, &chat, &cfg, id, true)
                    .await
                    .unwrap();
                black_box(&report.summary_text);
                black_box(report.summary_tokens);
                black_box(report.quality_score);
                if should_assert() {
                    assert!(!report.summary_text.is_empty(), "summary must be generated");
                    assert!(report.summary_tokens > 0, "tokens must be counted");
                    // Pipeline sanity: the canned summary must score as a
                    // faithful (not perfect) compression of the source. The
                    // ≥ 0.85 acceptance gate lives in tests/summarize_quality.rs.
                    assert!(
                        report.quality_score >= 0.35,
                        "quality_score {} too low — summarizer or scorer regression",
                        report.quality_score
                    );
                }
            }
        })
    });
}

fn bench_summarize_speed(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("speed.db"));
    let rt = Runtime::new().unwrap();
    let store = rt.block_on(async { StoreHandle::open(&cfg, 2).await.unwrap() });
    let ids = rt.block_on(async {
        let mut ids = Vec::new();
        for text in [
            "Tokio is an asynchronous runtime for the Rust programming language.",
            "Rayon makes data parallelism in Rust simple and ergonomic.",
            "Serde serializes and deserializes Rust data structures efficiently.",
            "Cargo resolves and builds the dependency graph for Rust projects.",
        ] {
            ids.push(insert_old_memory(&store, text, "semantic").await);
        }
        ids
    });

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed(
        "Tokio async runtime, Rayon data parallelism, Serde serialization, Cargo build tool.",
    ));

    c.bench_function("summarize_speed", |b| {
        b.to_async(&rt).iter(|| {
            let chat = chat.clone();
            let store = store.clone();
            let cfg = cfg.clone();
            let ids = ids.clone();
            async move {
                let start = Instant::now();
                for id in &ids {
                    let _ = agos_memory::memory::summarize_by_id(&store, &chat, &cfg, *id, true)
                        .await
                        .unwrap();
                }
                black_box(start.elapsed());
            }
        })
    });
}

fn bench_batch_summarize(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("batch.db"));
    let rt = Runtime::new().unwrap();
    let store = rt.block_on(async { StoreHandle::open(&cfg, 2).await.unwrap() });
    rt.block_on(async {
        for i in 0..16 {
            insert_old_memory(
                &store,
                &format!("Batch summarization fixture number {i} about the system behavior."),
                "semantic",
            )
            .await;
        }
    });

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed(
        "Periodic fixture summary of the system behavior.",
    ));

    c.bench_function("batch_summarize_throughput", |b| {
        b.to_async(&rt).iter(|| {
            let chat = chat.clone();
            let store = store.clone();
            let cfg = cfg.clone();
            async move {
                clear_summaries(&store).await;
                let n = agos_memory::memory::run_summarization_job(&store, &chat, &cfg)
                    .await
                    .unwrap();
                black_box(n);
            }
        })
    });
}

fn bench_dedup_consolidation(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("dedup.db"));
    let rt = Runtime::new().unwrap();
    let store = rt.block_on(async { StoreHandle::open(&cfg, 2).await.unwrap() });
    rt.block_on(async {
        // Four identical texts → one cluster with three absorbable newcomers.
        for _ in 0..4 {
            insert_old_memory(
                &store,
                "Rust is a systems programming language focused on safety.",
                "semantic",
            )
            .await;
        }
        for i in 0..12 {
            insert_old_memory(
                &store,
                &format!("Distinct dedup fixture {i} with unrelated wording."),
                "semantic",
            )
            .await;
        }
    });

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed("Dedup fixture summary."));

    c.bench_function("dedup_consolidation_speed", |b| {
        b.to_async(&rt).iter(|| {
            let chat = chat.clone();
            let store = store.clone();
            let cfg = cfg.clone();
            async move {
                let report = agos_memory::memory::run_consolidation_job(&store, &chat, &cfg)
                    .await
                    .unwrap();
                black_box(report);
            }
        })
    });
}

fn bench_ttl_reaper(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("ttl.db"));
    let rt = Runtime::new().unwrap();
    let store = rt.block_on(async { StoreHandle::open(&cfg, 2).await.unwrap() });
    rt.block_on(async {
        insert_ttl_memories(&store, agos_memory::util::SystemClock.now_millis(), 30).await;
    });

    c.bench_function("ttl_reaper_throughput", |b| {
        b.to_async(&rt).iter(|| {
            let store = store.clone();
            let cfg = cfg.clone();
            async move {
                reset_ttl_status(&store).await;
                let n = ttl_reaper_pass(&store, &cfg).await;
                black_box(n);
            }
        })
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_summarize_quality,
              bench_summarize_speed,
              bench_batch_summarize,
              bench_dedup_consolidation,
              bench_ttl_reaper
);
criterion_main!(benches);
