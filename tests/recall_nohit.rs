//! 0035 acceptance: no-hit scenario (D26).
//!
//! When nothing scores at or above `min_score`, the report is empty and flagged
//   as `no_hit = true` — the caller decides what to do, recall never panics.

mod common;

use agos_memory::config::RecallConfig;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, RecallReport, render_no_hit};
use agos_memory::storage::StoreHandle;
use agos_memory::util::Clock;
use common::{NormEmbedder, store};

const QUERY: &str = "vehicle maintenance log";

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
            // Insert a unit vector for vec leg matching
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
async fn no_hit_below_min_score() -> Result<()> {
    let (store, _dir) = store("nohit-min-score").await;
    let embedder = NormEmbedder::new(1536);

    // Seed a memory that matches the query
    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    // Set min_score above what the memory can achieve
    let mut q = query();
    q.min_score = 0.99; // effectively impossible to reach
    q.budget_tokens = 200;

    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    assert!(report.no_hit, "report should be flagged as no_hit");
    assert!(report.hits.is_empty(), "no hits should be returned");
    assert_eq!(report.tokens_used, 0, "no tokens used");
    Ok(())
}

#[tokio::test]
async fn no_hit_with_no_matching_memories() -> Result<()> {
    let (store, _dir) = store("nohit-no-memories").await;
    let embedder = NormEmbedder::new(1536);

    // No memories seeded at all
    let mut q = query();
    q.min_score = 0.0;
    q.budget_tokens = 200;

    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    assert!(
        report.no_hit,
        "report should be flagged as no_hit when no memories exist"
    );
    assert!(report.hits.is_empty(), "no hits should be returned");
    Ok(())
}

#[tokio::test]
async fn no_hit_renders_correct_message() -> Result<()> {
    let _report = RecallReport {
        no_hit: true,
        hits: vec![],
        degraded: false,
        latency_ms: 5,
        ..Default::default()
    };

    let out = render_no_hit("test query", 0.35, 0);
    assert!(out.contains("No useful memories for \"test query\""));
    assert!(out.contains("min_score=0.35"));
    assert!(out.contains("candidates=0"));
    Ok(())
}

#[tokio::test]
async fn no_hit_with_min_score_filters_results() -> Result<()> {
    let (store, _dir) = store("nohit-min-score-filters").await;
    let embedder = NormEmbedder::new(1536);

    // Seed a matching memory
    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    // Set min_score high enough to filter out the match
    let mut q = query();
    q.min_score = 0.99;
    q.budget_tokens = 200;

    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    assert!(report.no_hit, "should be no-hit when min_score filters all");
    assert!(report.hits.is_empty(), "no hits should be returned");
    Ok(())
}

#[tokio::test]
async fn no_hit_with_high_min_score_on_matching_memory() -> Result<()> {
    let (store, _dir) = store("nohit-high-min").await;
    let embedder = NormEmbedder::new(1536);

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    // Memory exists and would match, but min_score is too high
    let mut q = query();
    q.min_score = 0.99; // above achievable score
    q.budget_tokens = 200;

    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    assert!(report.no_hit);
    assert!(report.hits.is_empty());
    // The candidate count should reflect that candidates existed but were filtered out by min_score
    Ok(())
}
