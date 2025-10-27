//! Token-budget packing acceptance tests (issue 0034, decision D25).
//!
//! Packing decides *what the user actually sees*, so its contract is strict:
//!
//! - the `budget_tokens` ceiling is honored **exactly** (`tokens_used` ≤ budget);
//! - items are dropped **whole** — never a truncated fragment;
//! - each tier gets its declared share, with rollover in declared order;
//! - `pinned` items get first claim on the budget, even past their tier slice;
//! - a summary may be swapped in for a full text that does not fit, and the
//!   hit records `Placement::Summary` so the caller can fence it.
//!
//! The tests control size *exactly*: the [`HeuristicCounter`] rate
//! (`ceil(chars/4) · 1.15`) is deterministic, so texts are built by repeating
//! words until the counter reports the wanted token count, and every
//! expectation in a test is computed through the same counter — no magic
//! numbers. Each seed takes a one-character tag that prefixes every word in
//! its text: same-length words mean equal costs across tags, while distinct
//! texts mean distinct `public_id`s (same-size seeds must not collide). The
//! [`AxisEmbedder`] keeps similarity under control (every seeded row at 0°
//! from the query vector), and seeded texts avoid the query token so the FTS
//! leg stays empty and the candidate set is exactly what was seeded.

mod common;

use common::{DIM, store};

use async_trait::async_trait;

use agos_memory::config::{BudgetSplit, Config};
use agos_memory::embed::Embedder;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::tokens::{HeuristicCounter, TokenCounter};
use agos_memory::util::{Clock, SystemClock, sha256_hex};

/// Query token. No seeded row contains it, so the FTS leg stays empty.
const QUERY: &str = "quartz";

/// The same counter recall packs with; expectations are computed through it.
fn counter() -> HeuristicCounter {
    HeuristicCounter::new()
}

/// Text of at least `target` tokens under [`counter`], built from same-length
/// tagged words (`{tag}0`, `{tag}1`, …) so different tags cost the same while
/// producing different texts. Tagged words can never match the query token.
fn text_of(target: u64, tag: char) -> String {
    let c = counter();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0u32;
    while c.count(&words.join(" ")) < target {
        words.push(format!("{tag}{i}"));
        i += 1;
    }
    words.join(" ")
}

/// Token cost of a text, measured the way packing measures it.
fn cost(text: &str) -> u64 {
    counter().count(text)
}

/// Public id derived from the text, so seeding is deterministic.
fn public_id(text: &str) -> String {
    format!("pub_{}", &sha256_hex(text)[..12])
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Embedder returning one fixed unit vector (`e0`) for *any* text, so a row
/// seeded at 0° is retrieved with similarity 1.0. Note: with this embedder all
/// candidates tie, so *which* of two equal candidates is placed first is an
/// implementation detail — tests must assert invariants, not tie order.
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

fn e0() -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[0] = 1.0;
    v
}

/// One seeded row. Packing only cares about tier, pin state, text and summary;
/// similarity is 0° for everything so retrieval is never the variable.
struct Seed {
    text: String,
    summary_text: Option<String>,
    summary_tokens: Option<i64>,
    tier: String,
    pinned: bool,
}

impl Seed {
    /// A fresh, active, trusted row of `tokens` tokens in the `semantic` tier.
    fn new(tokens: u64, tag: char) -> Self {
        Self {
            text: text_of(tokens, tag),
            summary_text: None,
            summary_tokens: None,
            tier: "semantic".to_string(),
            pinned: false,
        }
    }

    /// Attach a summary and record its token cost the way the writer would.
    fn with_summary(mut self, tokens: u64) -> Self {
        let s = text_of(tokens, self.text.chars().next().unwrap_or('s'));
        self.summary_tokens = Some(cost(&s) as i64);
        self.summary_text = Some(s);
        self
    }

    fn tier(mut self, v: &str) -> Self {
        self.tier = v.to_string();
        self
    }

    fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }
}

/// Insert a row and its `vec0` row by direct SQL, returning `public_id`.
///
/// Bypassing the write path is deliberate: the packing decision must be a
/// function of the seeded columns, not of whatever the writer happens to set.
async fn seed(store: &StoreHandle, s: Seed) -> String {
    let text = s.text.clone();
    let hash = sha256_hex(&text);
    let pid = public_id(&text);
    let pid_out = pid.clone();
    let vector = blob(&e0());
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                 status, trust, importance_current, confidence, pinned,
                 expires_at, last_referenced_at, created_at, updated_at,
                 summary_text, summary_tokens)
                 VALUES (?1, 'default', ?2, 'fact', ?3, ?4, 'active', 'trusted', 0.5, 1.0, ?5,
                 NULL, NULL, ?6, ?6, ?7, ?8)",
                rusqlite::params![
                    pid,
                    s.tier,
                    text,
                    hash,
                    s.pinned as i64,
                    SystemClock.now_millis(),
                    s.summary_text,
                    s.summary_tokens,
                ],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, 0, 0, 'fact', ?4)",
                rusqlite::params![id, vector, s.tier, s.pinned as i64],
            )?;
            Ok(())
        })
        .await
        .expect("seed insert");
    pid_out
}

/// A query with the configured defaults, overridden per test.
fn query() -> RecallQuery {
    RecallQuery::new(QUERY, &Config::default().recall)
}

/// Give one tier the whole budget (kills the split as a variable).
fn all_to(q: &mut RecallQuery, tier: &str) {
    q.budget_split = BudgetSplit {
        working: if tier == "working" { 1.0 } else { 0.0 },
        episodic: if tier == "episodic" { 1.0 } else { 0.0 },
        semantic: if tier == "semantic" { 1.0 } else { 0.0 },
        procedural: if tier == "procedural" { 1.0 } else { 0.0 },
    };
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

/// The ceiling is a hard invariant: whatever packing decides, the total
/// injected tokens never exceed `budget_tokens`, and `tokens_used` equals the
/// sum of the placed hits' own counts.
#[tokio::test]
async fn ceiling_is_honored_exactly() {
    let (store, _dir) = store("budget_ceiling").await;

    // Three ~800-token items against a 1500-token budget: at most one fits
    // whole (800 + 800 > 1500), so packing must stop well short of 2400.
    let a = seed(&store, Seed::new(800, 'a')).await;
    let b = seed(&store, Seed::new(800, 'b')).await;
    let c = seed(&store, Seed::new(800, 'c')).await;

    let mut q = query();
    q.budget_tokens = 1500;
    all_to(&mut q, "semantic");
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    assert!(
        report.tokens_used <= 1500,
        "tokens_used {} exceeds the ceiling",
        report.tokens_used
    );
    let placed: u64 = report
        .hits
        .iter()
        .filter(|h| h.injected())
        .map(|h| h.tokens)
        .sum();
    assert_eq!(
        report.tokens_used, placed,
        "tokens_used must equal the sum of placed tokens"
    );
    assert!(
        placed >= cost(&text_of(800, 'a')),
        "at least the best item fits"
    );
    assert!(
        report.tier_tokens.semantic == placed,
        "single-tier run: every placed token belongs to semantic"
    );
    // The dropped items are marked whole-drop, with zero tokens.
    for pid in [&a, &b, &c] {
        let h = hit(&report, pid);
        if !h.injected() {
            assert!(
                matches!(h.placement, agos_memory::recall::Placement::Dropped(_)),
                "unplaced hit must be Dropped, got {:?}",
                h.placement
            );
            assert_eq!(h.tokens, 0, "dropped hits contribute nothing");
        }
    }
}

/// D25: an item that does not fit is dropped **whole**. The two items cannot
/// both fit (1200 + 600 > 1500), so one is placed whole and the other is
/// dropped whole — with equal similarity the placement order among ties is an
/// implementation detail, but *no item may ever appear truncated*.
#[tokio::test]
async fn oversized_item_dropped_whole_not_truncated() {
    let (store, _dir) = store("budget_whole_drop").await;

    let big = seed(&store, Seed::new(1200, 'b')).await;
    let small = seed(&store, Seed::new(600, 's')).await;

    let mut q = query();
    q.budget_tokens = 1500;
    all_to(&mut q, "semantic");
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    let big_hit = hit(&report, &big);
    let small_hit = hit(&report, &small);

    // Exactly one fits whole; the loser is dropped, contributes nothing.
    let placed_count = [&big_hit, &small_hit]
        .iter()
        .filter(|h| h.injected())
        .count();
    assert_eq!(placed_count, 1, "only one of the two items fits 1500");
    assert!(
        report.tokens_used <= 1500,
        "tokens_used {} exceeds the ceiling",
        report.tokens_used
    );
    for h in [&big_hit, &small_hit] {
        if h.injected() {
            assert_eq!(
                h.placement,
                agos_memory::recall::Placement::Full,
                "a placed item is never truncated, got {:?}",
                h.placement
            );
        } else {
            assert!(
                matches!(h.placement, agos_memory::recall::Placement::Dropped(_)),
                "the loser is dropped whole, got {:?}",
                h.placement
            );
            assert_eq!(h.tokens, 0, "dropped item contributes no tokens");
        }
    }
    assert_eq!(
        report.tokens_used,
        big_hit.tokens.max(small_hit.tokens),
        "tokens_used is exactly the placed item's cost"
    );
}

/// D25: each tier gets its declared share. Two working items (250 each) fit
/// the working slice; only one of two semantic items (300 each) fits the
/// semantic slice — and the leftover semantic item must not steal the working
/// tier's budget.
#[tokio::test]
async fn tier_split_is_respected() {
    let (store, _dir) = store("budget_split").await;

    let w1 = seed(&store, Seed::new(250, 'a').tier("working")).await;
    let w2 = seed(&store, Seed::new(250, 'b').tier("working")).await;
    let s1 = seed(&store, Seed::new(300, 'c')).await;
    let s2 = seed(&store, Seed::new(300, 'd')).await;

    let mut q = query();
    q.budget_tokens = 1000;
    // working 0.6 → 600 (both 250s fit), semantic 0.4 → 400 (one 300 fits).
    q.budget_split = BudgetSplit {
        working: 0.6,
        episodic: 0.0,
        semantic: 0.4,
        procedural: 0.0,
    };
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    for pid in [&w1, &w2, &s1, &s2] {
        let h = hit(&report, pid);
        let expected_share: u64 = if h.tier == "working" { 600 } else { 400 };
        assert!(
            h.injected() || h.tokens == 0,
            "{pid} must be either placed or dropped whole"
        );
        if h.injected() {
            assert!(
                report.tier_tokens.get(&h.tier) <= expected_share + 2,
                "tier {} placed {} tokens against a ~{} share",
                h.tier,
                report.tier_tokens.get(&h.tier),
                expected_share
            );
        }
    }

    assert_eq!(
        report.tier_tokens.get("working"),
        cost(&text_of(250, 'a')) * 2,
        "both working items placed within the working share"
    );
    let semantic_placed = [&s1, &s2]
        .iter()
        .filter(|p| hit(&report, p).injected())
        .count();
    assert_eq!(
        semantic_placed, 1,
        "only one 300-token semantic item fits the 400-token share"
    );

    // The unplaced semantic item is recorded as a tier-budget drop.
    let loser = if hit(&report, &s1).injected() {
        &s2
    } else {
        &s1
    };
    assert_eq!(
        hit(&report, loser).placement,
        agos_memory::recall::Placement::Dropped(agos_memory::recall::DropReason::TierBudget),
        "the semantic loser was cut by its tier share, not the global budget"
    );
}

/// D25: a pinned item gets first claim on the budget — even when it exceeds
/// its own tier's slice — and later items absorb whatever is left.
#[tokio::test]
async fn pinned_item_placed_despite_tier_slice() {
    let (store, _dir) = store("budget_pinned").await;

    // Semantic share is 200; the pinned item needs 500. It must still be
    // placed (first claim, drawing on the unclaimed tiers' rollover), and the
    // 800-token regular item must then find only 500 left.
    let pin = seed(&store, Seed::new(500, 'p').pinned()).await;
    let regular = seed(&store, Seed::new(800, 'r')).await;

    let mut q = query();
    q.budget_tokens = 1000;
    q.budget_split = BudgetSplit {
        working: 0.0,
        episodic: 0.0,
        semantic: 0.2,
        procedural: 0.0,
    };
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    let pin_hit = hit(&report, &pin);
    assert_eq!(
        pin_hit.placement,
        agos_memory::recall::Placement::Full,
        "the pinned item is placed despite blowing its tier slice"
    );
    assert_eq!(pin_hit.tokens, cost(&text_of(500, 'p')));
    assert_eq!(
        report.tokens_used,
        cost(&text_of(500, 'p')),
        "the 800-token regular item does not fit in the remainder"
    );
    let reg_hit = hit(&report, &regular);
    assert!(
        matches!(
            reg_hit.placement,
            agos_memory::recall::Placement::Dropped(_)
        ),
        "the regular item is dropped once the pin has claimed its budget"
    );
}

/// D25 summary-swap: when the full text does not fit but the summary does, the
/// summary is placed, marked `Placement::Summary`, and the full text is not.
#[tokio::test]
async fn summary_swaps_in_when_full_text_does_not_fit() {
    let (store, _dir) = store("budget_summary_swap").await;

    // Full text 1200 tokens, summary 100; budget 500. The full text cannot
    // fit whole — and D25 forbids fragments — so the summary is swapped in.
    let item = seed(&store, Seed::new(1200, 'f').with_summary(100)).await;

    let mut q = query();
    q.budget_tokens = 500;
    all_to(&mut q, "semantic");
    let report = recall(&store, &AxisEmbedder, &q).await.unwrap();

    let h = hit(&report, &item);
    assert_eq!(
        h.placement,
        agos_memory::recall::Placement::Summary,
        "the summary was swapped in for the oversized full text"
    );
    assert_eq!(
        h.tokens,
        cost(&text_of(100, 'f')),
        "the summary's measured cost is what is booked"
    );
    assert!(h.injected(), "a summary hit counts as injected");
    assert_eq!(report.tokens_used, h.tokens);
    assert!(report.tokens_used <= 500);
}
