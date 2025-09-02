//! Rerank + decay acceptance tests (issue 0033, decisions D23/D24/D25/D28).
//!
//! Rerank exists because RRF answers the wrong question. Fusion asks "which
//! candidates did the two legs agree on"; the user wants "which of them is
//! worth injecting now". The score is the D23 blend
//!
//! ```text
//! score = 0.60·sim_norm + 0.25·importance_eff + 0.15·decay(t)
//! ```
//!
//! To test that blend the tests must control each term *independently*:
//!
//! - **`sim_norm`**: [`AxisEmbedder`] returns one fixed unit vector for the
//!   query, and each row's vector is seeded at a chosen angle from it, so the
//!   cosine similarity is exactly `cos(deg)` and the KNN order is by
//!   construction. No provider, no guessing.
//! - **`decay`**: driven through `created_at`/`last_referenced_at` and the tier
//!   half-life, both of which the tests set explicitly.
//! - **`importance_eff`**: `importance_current` (and `confidence`, for
//!   `pending`).
//!
//! Every seeded text deliberately avoids the query token, so the FTS leg
//! returns nothing and the vec leg alone drives `rrf`. That keeps the
//! similarity term a clean function of the seeded angle instead of a mix of
//! KNN rank and BM25 rank.

mod common;

use common::{DIM, store};

use async_trait::async_trait;

use agos_memory::config::Config;
use agos_memory::embed::Embedder;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{Clock, SystemClock, sha256_hex};

/// Query token. No seeded row contains it, so the FTS leg stays empty.
const QUERY: &str = "quartz";

/// Public id derived from the text, so seeding is deterministic.
fn public_id(text: &str) -> String {
    format!("pub_{}", &sha256_hex(text)[..12])
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Embedder that returns one fixed unit vector (`e0`) for *any* text.
///
/// The query vector is therefore known exactly, and a row seeded with a unit
/// vector at angle `deg` in the `e0`–`e1` plane has cosine similarity
/// `cos(deg)` — i.e. `sim = 1 - distance = cos(deg)`.
struct AxisEmbedder;

#[async_trait]
impl Embedder for AxisEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| e0()).collect())
    }

    fn model(&self) -> &str {
        "axis-mock"
    }

    fn dim(&self) -> usize {
        DIM
    }
}

/// The query vector: unit vector on axis 0.
fn e0() -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = 1.0;
    v
}

/// Unit vector at `deg` degrees from [`e0`] within the `e0`–`e1` plane.
///
/// Cosine similarity to [`e0`] is exactly `cos(deg)`, so `deg` *is* the knob
/// for the `sim` term (0° = identical, 90° = orthogonal).
fn vec_at(deg: f64) -> Vec<f32> {
    let r = deg.to_radians();
    let mut v = vec![0.0f32; DIM];
    v[0] = r.cos() as f32;
    v[1] = r.sin() as f32;
    v
}

/// One seeded row, with only the fields the score depends on made explicit.
struct Seed {
    text: String,
    tier: String,
    status: String,
    trust: String,
    importance: f64,
    confidence: f64,
    created_at: i64,
    last_referenced_at: Option<i64>,
    /// Angle from the query vector; cosine similarity is `cos(deg)`.
    deg: f64,
}

impl Seed {
    /// A fresh, active, trusted `semantic` row of middling importance. Callers
    /// override exactly the term they are testing.
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            tier: "semantic".to_string(),
            status: "active".to_string(),
            trust: "trusted".to_string(),
            importance: 0.5,
            confidence: 1.0,
            created_at: SystemClock.now_millis(),
            last_referenced_at: None,
            deg: 0.0,
        }
    }

    fn tier(mut self, v: &str) -> Self {
        self.tier = v.to_string();
        self
    }

    fn status(mut self, v: &str) -> Self {
        self.status = v.to_string();
        self
    }

    fn importance(mut self, v: f64) -> Self {
        self.importance = v;
        self
    }

    fn confidence(mut self, v: f64) -> Self {
        self.confidence = v;
        self
    }

    /// Created `days` ago, so decay has something to act on.
    fn aged_days(mut self, days: i64) -> Self {
        self.created_at = SystemClock.now_millis() - days * 24 * 60 * 60 * 1000;
        self
    }

    fn last_referenced_days_ago(mut self, days: i64) -> Self {
        self.last_referenced_at = Some(SystemClock.now_millis() - days * 24 * 60 * 60 * 1000);
        self
    }

    fn deg(mut self, v: f64) -> Self {
        self.deg = v;
        self
    }
}

/// Insert a row and its `vec0` row by direct SQL, returning `public_id`.
///
/// Bypassing the write path is deliberate: the score must be a function of the
/// seeded columns, not of whatever the writer happens to set.
async fn seed(store: &StoreHandle, s: Seed) -> String {
    let text = s.text.clone();
    let hash = sha256_hex(&text);
    let pid = public_id(&text);
    let pid_out = pid.clone();
    let vector = blob(&vec_at(s.deg));
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                 status, trust, importance_current, confidence,
                 expires_at, last_referenced_at, created_at, updated_at)
                 VALUES (?1, 'default', ?2, 'fact', ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10, ?10)",
                rusqlite::params![
                    pid,
                    s.tier,
                    text,
                    hash,
                    s.status,
                    s.trust,
                    s.importance,
                    s.confidence,
                    s.last_referenced_at,
                    s.created_at,
                ],
            )?;
            let id = conn.last_insert_rowid();
            let status_code = match s.status.as_str() {
                "active" => 0,
                "pending" => 1,
                "deprecated" => 2,
                _ => 3,
            };
            let trust_code = match s.trust.as_str() {
                "trusted" => 0,
                "untrusted" => 1,
                _ => 2,
            };
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'fact', 0)",
                rusqlite::params![id, vector, s.tier, status_code, trust_code],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    pid_out
}

// ---------------------------------------------------------------- helpers --

/// A D23-default query with `k = 8`, everything else from `[recall]` defaults.
///
/// Tests mutate exactly the knob they exercise, so a failure names the term
/// responsible instead of a shared fixture.
fn query() -> RecallQuery {
    RecallQuery::new(QUERY, &Config::default().recall)
}

/// Public ids in rank order (the report is already best-first).
fn ids(report: &agos_memory::recall::RecallReport) -> Vec<String> {
    report.hits.iter().map(|h| h.public_id.clone()).collect()
}

/// The hit for `pid`, or a panic naming the ids that *were* returned.
fn hit<'r>(
    report: &'r agos_memory::recall::RecallReport,
    pid: &str,
) -> &'r agos_memory::recall::RecallHit {
    report
        .hits
        .iter()
        .find(|h| h.public_id == pid)
        .unwrap_or_else(|| panic!("{pid} missing from {:?}", ids(report)))
}

/// Assert two floats are within `eps` (scores are float math, never exact).
fn close(a: f64, b: f64, eps: f64) {
    assert!((a - b).abs() < eps, "expected {a} ≈ {b} (±{eps})");
}

// ------------------------------------------------------------------ tests --

/// D24 end to end: a *dimmer* but fresh memory must outrank a *closer* but
/// stale one. This is the property that makes decay worth having — raw
/// similarity alone would keep returning the fossil.
///
/// Both rows are seeded so similarity and decay pull in opposite directions:
///
/// | row | angle | `sim` | tier | age | `HL` | decay |
/// |-----|-------|-------|------|-----|------|-------|
/// | A | 0° | 1.00 | episodic | 60 d | 21 d | ≈0.14 |
/// | B | 60° | 0.50 | working | fresh | 6 h | 1.00 |
///
/// `sim_norm` is the RRF term, and with a single populated leg A (rank 0) edges
/// B (rank 1): ≈0.500 vs ≈0.492. That 0.008 head start must not survive A's
/// 0.86-point decay deficit.
#[tokio::test]
async fn decay_beats_raw_similarity() {
    let (store, _dir) = store("rerank_decay").await;

    let stale = seed(
        &store,
        Seed::new("aged episodic fossil")
            .tier("episodic")
            .deg(0.0)
            .aged_days(60),
    )
    .await;
    let fresh = seed(
        &store,
        Seed::new("fresh working note").tier("working").deg(60.0),
    )
    .await;

    let mut q = query();
    q.include_episodic = true; // A's tier is opt-in (D26)
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    assert_eq!(
        ids(&report),
        vec![fresh.clone(), stale.clone()],
        "fresh working note must outrank the closer but 60-day-old episodic row"
    );

    // The mechanism, not just the ordering: decay is the term that flipped it.
    let a = hit(&report, &stale);
    let b = hit(&report, &fresh);
    assert!(
        a.components.decay < 0.2,
        "60d against a 21d half-life should decay hard, got {}",
        a.components.decay
    );
    close(b.components.decay, 1.0, 0.01);
    // Similarity really did favour A — otherwise the test would prove nothing.
    assert!(
        a.components.sim_norm > b.components.sim_norm,
        "A must start ahead on similarity ({} vs {})",
        a.components.sim_norm,
        b.components.sim_norm
    );
    assert!(b.score > a.score);
}

/// `semantic`/`procedural` default to an infinite half-life (D24): stable
/// knowledge must not rot just for being old. Age alone cannot move the score.
#[tokio::test]
async fn infinite_half_life_never_decays() {
    let (store, _dir) = store("rerank_infinite_hl").await;

    let ancient = seed(&store, Seed::new("timeless semantic fact").aged_days(400)).await;
    let recent = seed(&store, Seed::new("recent semantic fact")).await;

    let report = recall(&store, &AxisEmbedder, &query()).await.unwrap();

    close(hit(&report, &ancient).components.decay, 1.0, 1e-9);
    close(hit(&report, &recent).components.decay, 1.0, 1e-9);
    // Age contributes to neither row's score: identical decay (1.0) leaves only
    // the rank term and the (equal-by-default) importance in the D23 blend, so
    // the entire score gap must equal the `sim_norm` gap times `w_sim`.
    // Rank-based `sim_norm` cannot tie two rows even at identical similarity,
    // which is why the property is asserted on the *gap* rather than on
    // `sim_norm` equality. Had decay been keyed off `created_at`, the 400-day
    // age difference would appear here as a gap ~5× larger (0.15 · 0.9661).
    let a = hit(&report, &ancient);
    let b = hit(&report, &recent);
    let w_sim = f64::from(Config::default().recall.weights.sim);
    close(
        (a.score - b.score).abs(),
        w_sim * (a.components.sim_norm - b.components.sim_norm).abs(),
        1e-9,
    );
}

/// Decay is measured from `COALESCE(last_referenced_at, created_at)`: a memory
/// that was *used* yesterday is fresh even though it was *written* long ago.
///
/// The two rows below differ by 399 days of creation age yet must decay
/// identically — a year-old row referenced yesterday and a row created
/// yesterday are equally fresh. Decaying from `created_at` alone would bury
/// exactly the memories that keep proving useful (the fossil would score ~0.0
/// instead of ≈0.97).
#[tokio::test]
async fn last_referenced_resets_decay() {
    let (store, _dir) = store("rerank_last_referenced").await;

    // Written 400 days ago, but referenced 1 day ago.
    let reused = seed(
        &store,
        Seed::new("long-lived episodic habit")
            .tier("episodic")
            .aged_days(400)
            .last_referenced_days_ago(1),
    )
    .await;
    // Written 1 day ago, never referenced: the same "day of freshness".
    let created_yesterday = seed(
        &store,
        Seed::new("brand new episodic habit")
            .tier("episodic")
            .aged_days(1),
    )
    .await;

    let mut q = query();
    q.include_episodic = true; // episodic is opt-in (D26)
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    // Episodic half-life is 21 days, so one day of age is 0.5^(1/21) ≈ 0.9675.
    let expected = 0.5f64.powf(1.0 / 21.0);
    close(hit(&report, &reused).components.decay, expected, 1e-6);
    close(
        hit(&report, &created_yesterday).components.decay,
        expected,
        1e-6,
    );
    // The 400-day creation age is invisible: freshness is decay-relevant age.
    // The tolerance is 1e-6 rather than exact because each `seed` stamps its own
    // `now`, so the two rows' ages differ by the milliseconds between the calls.
    // A decay that wrongly used `created_at` would differ by ~0.97 here, not by
    // the ~3e-8 this tolerance absorbs.
    close(
        hit(&report, &reused).components.decay,
        hit(&report, &created_yesterday).components.decay,
        1e-6,
    );
}

/// D25/D28: a `pending` memory is unverified, so its importance is discounted
/// by extraction `confidence` — and only for `pending` rows.
///
/// The low-confidence row is deliberately given the *better* similarity (rank 0
/// vs rank 1), so if `confidence` were ignored it would win. The confidence
/// discount must be large enough to flip that ordering; on top of that, the
/// effective importance is asserted to be exactly `importance × confidence`.
#[tokio::test]
async fn pending_scored_by_importance_times_confidence() {
    let (store, _dir) = store("rerank_pending_confidence").await;

    // Better similarity (rank 0) but barely believed.
    let shaky = seed(
        &store,
        Seed::new("tentative pending guess")
            .status("pending")
            .importance(1.0)
            .confidence(0.1)
            .deg(0.0),
    )
    .await;
    // Worse similarity (rank 1) but fully believed.
    let solid = seed(
        &store,
        Seed::new("confident pending claim")
            .status("pending")
            .importance(1.0)
            .confidence(1.0)
            .deg(60.0),
    )
    .await;

    // Default: `pending` is not eligible at all (D28 opt-in).
    let report = recall(&store, &AxisEmbedder, &query()).await.unwrap();
    assert!(
        report.hits.is_empty(),
        "pending rows must be excluded without --include-pending, got {:?}",
        ids(&report)
    );

    let mut q = query();
    q.include_pending = true;
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    assert_eq!(
        ids(&report),
        vec![solid.clone(), shaky.clone()],
        "confidence must outweigh the similarity head start for pending rows"
    );

    // Confidence really did favour the loser — otherwise the test proves nothing.
    let a = hit(&report, &shaky);
    let b = hit(&report, &solid);
    assert!(
        a.components.sim_norm > b.components.sim_norm,
        "the discounted row must start ahead on similarity ({} vs {})",
        a.components.sim_norm,
        b.components.sim_norm
    );

    // The mechanism: effective importance is exactly importance × confidence.
    close(a.components.importance, 1.0 * 0.1, 1e-9);
    close(b.components.importance, 1.0 * 1.0, 1e-9);
    // Raw confidence is carried for explain (0035).
    close(a.components.confidence, 0.1, 1e-9);
    close(b.components.confidence, 1.0, 1e-9);
    assert!(b.score > a.score);
}
