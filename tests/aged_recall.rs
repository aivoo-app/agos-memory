//! v0.6.0 issue 0005 — aged facts lose rank but stay recallable; pins ignore age.

mod common;

use agos_memory::config::Config;
use agos_memory::embed::{Embedder, HashEmbedder};
use agos_memory::error::Result;
use agos_memory::memory;
use agos_memory::recall::{RecallQuery, RecallReport, recall_with_clock};
use agos_memory::storage::StoreHandle;
use agos_memory::util::clock::{Clock, FakeClock, SystemClock};
use common::{DIM, store};

const QUERY: &str = "project northstar";
const OLD_TEXT: &str = "project northstar launch decision original marker alpha beta";
const FRESH_TEXT: &str = "project northstar launch decision current marker gamma delta";

struct AxisEmbedder;

#[async_trait::async_trait]
impl Embedder for AxisEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| e0()).collect())
    }

    fn model(&self) -> &str {
        "axis-aged-mock"
    }

    fn dim(&self) -> usize {
        DIM
    }
}

fn e0() -> Vec<f32> {
    let mut vector = vec![0.0_f32; DIM];
    vector[0] = 1.0;
    vector
}

fn vector_blob() -> Vec<u8> {
    e0().iter().flat_map(|value| value.to_le_bytes()).collect()
}

fn query() -> RecallQuery {
    let mut config = Config::default().recall;
    config.include_episodic = true;
    config.budget_tokens = 500;
    RecallQuery::new(QUERY, &config)
}

async fn seed_real(store: &StoreHandle, text: &str) -> Result<String> {
    let dim = store.embed_dim().await?;
    let embedder = HashEmbedder::new(dim);
    let config = Config::default();
    let row = memory::remember(
        store,
        "episodic",
        "fact",
        text,
        "user",
        0.8,
        &embedder,
        "aged-recall-test-v1",
        config.memory.pending_threshold,
        config.memory.dedup_threshold,
    )
    .await?;
    Ok(row.public_id)
}

/// Normalize only the fields that control this test: age and the two retrieval
/// legs. The row was created through the real write path above.
async fn set_age_and_vector(
    store: &StoreHandle,
    public_id: &str,
    created_at: i64,
    pinned: bool,
) -> Result<()> {
    let public_id = public_id.to_string();
    let vector = vector_blob();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories
                 SET created_at = ?2, updated_at = ?2, last_referenced_at = NULL,
                     pinned = ?3
                 WHERE public_id = ?1",
                rusqlite::params![public_id, created_at, pinned as i64],
            )?;
            conn.execute(
                "UPDATE vec_memories
                 SET embedding = ?2, pinned = ?3
                 WHERE rowid = (SELECT id FROM memories WHERE public_id = ?1)",
                rusqlite::params![public_id, vector, pinned as i64],
            )?;
            Ok(())
        })
        .await
}

async fn ensure_pin(store: &StoreHandle, public_id: &str, now: i64) -> Result<()> {
    let public_id = public_id.to_string();
    store
        .write(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO pins(memory_id, pinned_at, by)
                 SELECT id, ?2, 'aged-recall-test' FROM memories WHERE public_id = ?1",
                rusqlite::params![public_id, now],
            )?;
            Ok(())
        })
        .await
}

fn hit<'a>(report: &'a RecallReport, public_id: &str) -> &'a agos_memory::recall::RecallHit {
    report
        .hits
        .iter()
        .find(|hit| hit.public_id == public_id)
        .unwrap_or_else(|| panic!("{public_id} missing from aged recall"))
}

#[tokio::test]
async fn six_month_fact_stays_reachable_but_loses_rank() -> Result<()> {
    let (store, _dir) = store("aged_recall.db").await;
    let old_id = seed_real(&store, OLD_TEXT).await?;
    let fresh_id = seed_real(&store, FRESH_TEXT).await?;
    let now = SystemClock.now_millis();

    let mut old_scores = Vec::new();
    for days in [90_i64, 180, 365] {
        let aged_at = now - days * 24 * 60 * 60 * 1_000;
        set_age_and_vector(&store, &old_id, aged_at, false).await?;
        set_age_and_vector(&store, &fresh_id, now, false).await?;
        let clock = FakeClock::at(now);
        let report = recall_with_clock(&store, &AxisEmbedder, &query(), &clock).await?;
        let old = hit(&report, &old_id);
        let fresh = hit(&report, &fresh_id);

        println!(
            "aged recall: days={days} old_score={:.6} fresh_score={:.6} old_decay={:.6} hits={}",
            old.score,
            fresh.score,
            old.components.decay,
            report.hits.len()
        );
        assert!(
            !report.no_hit,
            "aged fact must remain reachable at {days} days"
        );
        assert!(
            old.score < fresh.score,
            "aged fact must lose rank at {days} days: old={} fresh={}",
            old.score,
            fresh.score
        );
        assert_eq!(report.hits[0].public_id, fresh_id);
        old_scores.push(old.score);
    }

    assert!(old_scores[1] >= 0.35, "180-day fact must clear min_score");
    assert!(old_scores[0] > old_scores[1]);
    assert!(old_scores[1] > old_scores[2]);
    Ok(())
}

#[tokio::test]
async fn pinned_fact_ignores_age() -> Result<()> {
    let (store, _dir) = store("aged_pin.db").await;
    let public_id = seed_real(&store, "project northstar pinned launch decision").await?;
    let now = SystemClock.now_millis();
    set_age_and_vector(&store, &public_id, now, true).await?;
    ensure_pin(&store, &public_id, now).await?;

    let young = recall_with_clock(&store, &AxisEmbedder, &query(), &FakeClock::at(now)).await?;
    let young_hit = hit(&young, &public_id);
    let aged_at = now - 365 * 24 * 60 * 60 * 1_000;
    set_age_and_vector(&store, &public_id, aged_at, true).await?;
    let old = recall_with_clock(&store, &AxisEmbedder, &query(), &FakeClock::at(now)).await?;
    let old_hit = hit(&old, &public_id);

    println!(
        "pinned age: young_score={:.6} old_score={:.6} decay={:.6}",
        young_hit.score, old_hit.score, old_hit.components.decay
    );
    assert!((young_hit.score - old_hit.score).abs() < 1e-9);
    assert!((young_hit.components.decay - 1.0).abs() < 1e-9);
    assert!((old_hit.components.decay - 1.0).abs() < 1e-9);
    Ok(())
}
