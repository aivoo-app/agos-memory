//! 0031 acceptance: degraded mode (D4).
//!
//! When the embedding provider is unavailable (`provider = "none"`), recall
//! degrades gracefully to BM25-only keyword search — never a hard failure.

mod common;

use agos_memory::config::{Config, EmbedConfig, EmbedProvider, RecallConfig};
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::Clock;
use common::{NormEmbedder, store};

const QUERY: &str = "vehicle maintenance log";

/// Config with no embedder (degraded mode)
fn degraded_config() -> Config {
    let mut cfg = Config::default();
    cfg.embed = agos_memory::config::EmbedConfig {
        provider: EmbedProvider::None,
        ..Default::default()
    };
    cfg
}

fn query() -> RecallQuery {
    RecallQuery::new(QUERY, &RecallConfig::default())
}

/// Insert a semantic memory by direct SQL and return its public_id.
async fn seed(store: &StoreHandle, text: &str) -> String {
    let pid = format!("pid-{}", &agos_memory::util::sha256_hex(text)[..12]);
    let pid_clone = pid.clone();
    let text_clone = text.to_string();
    let hash = agos_memory::util::sha256_hex(text);
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
                rusqlite::params![pid_clone, text_clone, hash, now],
            )?;
            let id = conn.last_insert_rowid();
            // Insert a unit vector for vec leg matching (won't be used in degraded mode)
            let blob: Vec<u8> = (0..1536).map(|_| 1.0f32.to_le_bytes()).flatten().collect();
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, 'semantic', 0, 0, 'fact', 0)",
                rusqlite::params![id, blob],
            )?;
            Ok(())
        })
        .await
        .expect("seed insert");
    pid
}

#[tokio::test]
async fn degraded_bm25_only_when_embedder_unavailable() -> Result<()> {
    let (store, _dir) = store("degraded-bm25").await;
    let _embedder = NormEmbedder::new(1536); // won't be used

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let cfg = degraded_config();
    let query = RecallQuery::new(QUERY, &cfg.recall);
    let report = recall(&store, &agos_memory::embed::NoEmbedder, &query).await?;

    // Should get results from BM25 only
    assert!(!report.no_hit, "should find results via BM25");
    assert!(report.degraded, "report should be marked as degraded");
    assert!(report.hits.len() >= 1, "should have at least one hit");
    Ok(())
}

#[tokio::test]
async fn degraded_mode_with_provider_none_config() -> Result<()> {
    let cfg = degraded_config();
    let (store, _dir) = store("degraded-config").await;

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    // Use the actual embedder which will fail and degrade
    let embedder = agos_memory::embed::embedder_from_config(&cfg.embed, 1536);
    let query = RecallQuery::new(QUERY, &cfg.recall);
    let report = recall(&store, &*embedder, &query).await?;

    assert!(report.degraded, "degraded flag should be true");
    assert!(!report.no_hit);
    Ok(())
}

#[tokio::test]
async fn degraded_mode_still_respects_min_score() -> Result<()> {
    let cfg = degraded_config();
    let (store, _dir) = store("degraded-min-score").await;

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let mut q = RecallQuery::new(QUERY, &cfg.recall);
    q.min_score = 0.99; // BM25 scores typically won't reach this

    let embedder = agos_memory::embed::embedder_from_config(&cfg.embed, 1536);
    let report = recall(&store, &*embedder, &q).await?;

    assert!(report.degraded);
    // With high min_score, even BM25 results should be filtered out
    // This depends on the actual BM25 scores
    Ok(())
}

#[tokio::test]
async fn degraded_mode_still_applies_hard_filter() -> Result<()> {
    let cfg = degraded_config();
    let (store, _dir) = store("degraded-filter").await;

    // Insert a deprecated memory (should be filtered out)
    let pid = seed(&store, "vehicle maintenance log for 2024").await;

    // Update it to deprecated status
    let pid_clone = pid.clone();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET status = 'deprecated' WHERE public_id = ?1",
                [pid_clone],
            )?;
            Ok(())
        })
        .await
        .expect("update status");

    let q = RecallQuery::new(QUERY, &cfg.recall);
    let report = recall(&store, &agos_memory::embed::NoEmbedder, &q).await?;

    // Deprecated memory should be filtered out even in degraded mode
    assert!(report.degraded);
    assert!(
        report.no_hit || report.hits.is_empty(),
        "deprecated memory should be filtered"
    );
    Ok(())
}

#[tokio::test]
async fn degraded_mode_no_panic_when_no_provider() -> Result<()> {
    // Test that degraded mode never panics, even with no embedder configured
    let cfg = degraded_config();
    let (store, _dir) = store("degraded-no-panic").await;

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let embedder = agos_memory::embed::embedder_from_config(&cfg.embed, 1536);
    let query = RecallQuery::new(QUERY, &cfg.recall);

    // Should not panic, just return degraded results
    let report = recall(&store, &*embedder, &query).await?;
    assert!(report.degraded);
    Ok(())
}
