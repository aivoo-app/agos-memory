//! Summarization tests (issue 0041).
//!
//! Covers the two modes (periodic + on-demand), per-tier eligibility, and the
//! stored-summary contract that recall packing's summary-swap (D25, covered
//! end-to-end in `tests/recall_budget.rs`) consumes.

use agos_memory::config::Config;
use agos_memory::llm::{ChatClient, MockChat};
use agos_memory::storage::{MemoryRow, StoreHandle};
use std::sync::Arc;

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

/// Insert a memory; `old` backdates `created_at` past every tier's
/// `summarize_after_days` threshold so periodic passes pick it up.
async fn make_memory(store: &StoreHandle, text: &str, tier: &str, old: bool) -> MemoryRow {
    let row = store
        .insert_memory(agos_memory::storage::NewMemory {
            text: text.into(),
            tier: tier.into(),
            kind: "fact".into(),
            source_kind: "user".into(),
        })
        .await
        .unwrap();
    if old {
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
    row
}

fn chat() -> Arc<dyn ChatClient> {
    Arc::new(MockChat::fixed("Concise summary of the stored fact."))
}

#[tokio::test]
async fn periodic_job_picks_eligible_and_stores_summary() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("periodic.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let old1 = make_memory(&store, "old semantic fact one", "semantic", true).await;
    let old2 = make_memory(&store, "old working fact two", "working", true).await;
    // Fresh memory: not yet past `summarize_after_days` — must be skipped.
    let fresh = make_memory(&store, "fresh semantic fact three", "semantic", false).await;

    let n = agos_memory::memory::run_summarization_job(&store, &chat(), &cfg)
        .await
        .unwrap();
    assert_eq!(n, 2, "only the two backdated memories are eligible");

    for row in [&old1, &old2] {
        let updated = store
            .get_memory(row.public_id.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(
            updated.summary_text.is_some(),
            "{} must be summarized",
            row.public_id
        );
        assert!(updated.summary_tokens > 0);
    }
    let fresh_row = store
        .get_memory(fresh.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert!(
        fresh_row.summary_text.is_none(),
        "fresh memory must not be summarized by the periodic pass"
    );

    // Idempotent: the second pass has nothing left to do.
    let n2 = agos_memory::memory::run_summarization_job(&store, &chat(), &cfg)
        .await
        .unwrap();
    assert_eq!(n2, 0, "re-running the periodic pass must be a no-op");
}

#[tokio::test]
async fn on_demand_summarize_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("ondemand.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    // On-demand has no age requirement: a fresh memory is summarizable.
    let row = make_memory(&store, "fresh fact summarized on demand", "semantic", false).await;

    let report = agos_memory::memory::summarize_by_id(&store, &chat(), &cfg, row.id, false)
        .await
        .unwrap();
    assert!(!report.summary_text.is_empty());
    assert!(report.summary_tokens > 0);
    assert!((0.0..=1.0).contains(&report.quality_score));
    assert!(!report.overwrote);

    let updated = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.summary_text.as_deref(),
        Some(report.summary_text.as_str())
    );
}

#[tokio::test]
async fn summarize_tier_batches_and_respects_config() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("tier.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    make_memory(&store, "semantic batch fact one", "semantic", true).await;
    make_memory(&store, "semantic batch fact two", "semantic", true).await;
    make_memory(&store, "working batch fact three", "working", true).await;

    let semantic = agos_memory::memory::summarize_tier(&store, &chat(), &cfg, "semantic")
        .await
        .unwrap();
    assert_eq!(
        semantic.len(),
        2,
        "both eligible semantic memories summarized"
    );

    let working = agos_memory::memory::summarize_tier(&store, &chat(), &cfg, "working")
        .await
        .unwrap();
    assert_eq!(working.len(), 1, "tiers are batched independently");

    // Idempotent within the tier.
    let again = agos_memory::memory::summarize_tier(&store, &chat(), &cfg, "semantic")
        .await
        .unwrap();
    assert!(
        again.is_empty(),
        "already-summarized tier must yield nothing"
    );
}

#[tokio::test]
async fn stored_summary_feeds_recall_packing() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_config(&dir.path().join("packing.db"));
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let row = make_memory(&store, "packing summary source fact", "semantic", true).await;
    agos_memory::memory::summarize_by_id(&store, &chat(), &cfg, row.id, false)
        .await
        .unwrap();

    // The D25 summary-swap consumer reads exactly these two columns; assert
    // the stored contract (swap placement itself is covered by
    // tests/recall_budget.rs::summary_swaps_in_when_full_text_does_not_fit).
    let updated = store
        .get_memory(row.public_id.clone())
        .await
        .unwrap()
        .unwrap();
    let summary = updated.summary_text.expect("summary stored");
    assert!(!summary.is_empty());
    assert!(updated.summary_tokens > 0, "tokens stored for budgeting");
    // Summary must be shorter than the source — the whole point of a summary.
    assert!(
        updated.summary_tokens < updated.text.len() as i64,
        "summary tokens must be below the source char count"
    );
}
