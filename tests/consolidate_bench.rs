//! Consolidation throughput gates (issues 0049/0050).
//!
//! Measured timings via `std::time::Instant` with **rate** assertions, not
//! vague wall-clock guesses:
//!
//! - batch summarize ≥ 100 memories/s (0049 target)
//! - TTL reaper ≥ 1000 memories/s (v0.4.0 milestone acceptance)
//!
//! Rate assertions apply to release runs only (`should_assert`, mirroring
//! `benches/recall_bench.rs`); debug runs keep lenient completion bounds so
//! `cargo test --all-targets` stays informative without being flaky.

use std::time::Instant;

use agos_memory::config::{Config, EmbedConfig, EmbedProvider};
use agos_memory::llm::{ChatClient, MockChat};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{Clock, SystemClock};
use std::sync::Arc;
use tempfile::tempdir;

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
            // Short working TTL so the reaper benches have expired rows.
            ttl_working_days: 3,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Bulk-insert `n` memories with `created_at` 31 days in the past, in one
/// writer transaction (per-row actor round-trips would dominate the rate).
async fn insert_expired_memories(store: &StoreHandle, now_millis: i64, n: usize) {
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
                    rusqlite::params![format!("ttl-rate-{}", i), "working", text, hash, created],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn summarize_throughput() {
    let dir = tempdir().unwrap();
    let cfg = test_config(&dir.path().join("summary.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed("Rust systems programming language."));

    // Insert 8 memories.
    for i in 0..8 {
        store
            .insert_memory(agos_memory::storage::NewMemory {
                text: format!("Memory {i}: Rust systems programming fact number {i}."),
                tier: "semantic".into(),
                kind: "fact".into(),
                source_kind: "user".into(),
            })
            .await
            .unwrap();
    }

    let start = Instant::now();
    let mut ids_seen = 0usize;
    for id in 1..=8i64 {
        let report = agos_memory::memory::summarize_by_id(&store, &chat, &cfg, id, true)
            .await
            .unwrap();
        ids_seen += 1;
        assert!(!report.summary_text.is_empty());
        assert!(report.summary_tokens > 0);
        assert!(
            report.quality_score > 0.0,
            "quality_score must be computed (got {})",
            report.quality_score
        );
    }
    let elapsed = start.elapsed();
    assert_eq!(ids_seen, 8);

    // 8 summaries through MockChat should complete well under 2 seconds.
    assert!(
        elapsed.as_secs_f64() < 2.0,
        "summarizing 8 memories took {:.2}s (threshold 2.0s)",
        elapsed.as_secs_f64()
    );
}

#[tokio::test]
async fn batch_summarize_meets_rate_target() {
    let dir = tempdir().unwrap();
    let cfg = test_config(&dir.path().join("batch-rate.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed("Fixture summary of the fact."));

    // 100 memories, backdated so the periodic pass is eligible for all tiers.
    for i in 0..100 {
        let row = store
            .insert_memory(agos_memory::storage::NewMemory {
                text: format!("Batch summarize fixture {i} with distinct wording {i}."),
                tier: if i % 2 == 0 { "semantic" } else { "episodic" }.into(),
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
    }

    let start = Instant::now();
    let n = agos_memory::memory::run_summarization_job(&store, &chat, &cfg)
        .await
        .unwrap();
    let elapsed = start.elapsed().as_secs_f64();
    assert_eq!(n, 100, "every eligible memory must be summarized");

    if should_assert() {
        let rate = n as f64 / elapsed;
        assert!(
            rate >= 100.0,
            "batch summarize rate {rate:.1} mem/s below the 100 mem/s target (0049)"
        );
    } else {
        assert!(
            elapsed < 10.0,
            "batch summarize of 100 memories took {elapsed:.2}s (debug bound 10s)"
        );
    }
}

#[tokio::test]
async fn dedup_consolidation_throughput() {
    let dir = tempdir().unwrap();
    let cfg = test_config(&dir.path().join("consolidation.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed("Fixture summary of the fact."));

    // 16 distinct memories — measures the scan + pair scoring + cleanup cost.
    for i in 0..16 {
        store
            .insert_memory(agos_memory::storage::NewMemory {
                text: format!("Memory number {i} contains important system information."),
                tier: "semantic".into(),
                kind: "fact".into(),
                source_kind: "user".into(),
            })
            .await
            .unwrap();
    }

    let start = Instant::now();
    let report = agos_memory::memory::run_consolidation_job(&store, &chat, &cfg)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        report.dedup_clusters_merged, 0,
        "distinct texts never merge"
    );
    assert!(
        elapsed.as_secs_f64() < 1.0,
        "consolidation of 16 memories took {:.2}s (threshold 1.0s)",
        elapsed.as_secs_f64()
    );
}

#[tokio::test]
async fn ttl_reaper_meets_rate_target() {
    let dir = tempdir().unwrap();
    let cfg = test_config(&dir.path().join("ttl.db"));
    let store = StoreHandle::open(&cfg, 4).await.unwrap();

    let now_millis = SystemClock.now_millis();
    const N: usize = 1000;
    insert_expired_memories(&store, now_millis, N).await;

    let start = Instant::now();
    let expired = store
        .expired_memories(
            now_millis,
            cfg.memory.ttl_working_days as i64,
            cfg.memory.ttl_episodic_days as i64,
            cfg.memory.ttl_semantic_days as i64,
            cfg.memory.ttl_procedural_days as i64,
        )
        .await
        .unwrap();
    assert_eq!(expired.len(), N, "all inserted memories must be expired");
    for (id, _, _) in &expired {
        store.deprecate_for_ttl(*id).await.unwrap();
    }
    let elapsed = start.elapsed().as_secs_f64();

    if should_assert() {
        let rate = N as f64 / elapsed;
        assert!(
            rate >= 1000.0,
            "TTL reaper rate {rate:.1} mem/s below the 1000 mem/s acceptance target"
        );
    } else {
        assert!(
            elapsed < 30.0,
            "reaper pass over {N} memories took {elapsed:.2}s (debug bound 30s)"
        );
    }
}

#[tokio::test]
async fn consolidation_scales_with_memory_count() {
    let dir = tempdir().unwrap();
    let cfg = test_config(&dir.path().join("scale.db"));
    let store = StoreHandle::open(&cfg, 4).await.unwrap();

    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed("Fixture summary of the fact."));

    // 32 memories to test scaling (dedup is pairwise per tier), backdated so
    // the periodic summarization pass is eligible.
    for i in 0..32 {
        let row = store
            .insert_memory(agos_memory::storage::NewMemory {
                text: format!("Scale test memory {i} with details about system behavior."),
                tier: "semantic".into(),
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
    }

    let start = Instant::now();
    let report = agos_memory::memory::run_consolidation_job(&store, &chat, &cfg)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(
        report.summaries_generated, 32,
        "all 32 memories must be summarized"
    );
    assert!(
        elapsed.as_secs_f64() < 1.5,
        "consolidation of 32 memories took {:.2}s (threshold 1.5s)",
        elapsed.as_secs_f64()
    );
}
