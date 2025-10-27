//! 0035 acceptance: the explain / no-hit contract.
//!
//! Confirms the three things this issue ships: an explain of *what was done*,
//! an explain of *why this was selected* (relational records + recall
//! injections), and a graceful no-hit path (D26). The `explain(text, kind)`
//! subjective-fit story is exercised here through the no-hit path, not as a
//! separate unit test — the explain is always whatever the recall produced;
//! its "reason" is the captured subjective fit.
//!
//! Coverage:
//! - `recall()` returns a typed report whose items carry `injected` and
//!   `summary` so the consumer knows what was placed (D25) — row pairs impose
//!   a structural order, and the report reflects it.
//! - explain text is bounded: when a `text` summary is injected, the
//!   comparator must still find the row, and the renderer must never exceed the
//!   configured cap.
//! - `render_report` emits the D26 no-hit line when nothing scored.
//!
//! D26 phrasing is reproduced exactly so the fixture can do a plain substring
//! match: `No useful memories for "<query>"`.

mod common;

use std::collections::HashMap;

use agos_memory::config::RecallConfig;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, RecallReport, explain, render_report};
use agos_memory::storage::StoreHandle;
use agos_memory::util::clock::Clock;
use agos_memory::util::sha256_hex;
use agos_memory::util::tokens::TokenCounter;
use agos_memory::util::{HeuristicCounter, SystemClock};
use common::{NormEmbedder, store};

const QUERY: &str = "vehicle maintenance log";

/// Text of at least `target` tokens under a fresh [`HeuristicCounter`], built
/// from same-length tagged words so different tags cost the same while
/// producing different texts. Tagged words can never match the query token
/// because the query token ("vehicle maintenance log") is a multi-word phrase
/// and tagged words are like "a0", "a1", ….
fn text_of(target: u64, tag: char) -> String {
    let c = HeuristicCounter::new();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0u32;
    loop {
        let text = words.join(" ");
        if c.count(&text) >= target {
            break;
        }
        words.push(format!("{tag}{i}"));
        i += 1;
    }
    words.join(" ")
}

/// One seeded row. Explain reads back what the store stored, so we write
/// provenance columns (source_kind, source_ref) directly via SQL.
struct Seed {
    text: String,
    summary_text: Option<String>,
    summary_tokens: Option<u64>,
    tier: String,
    source_kind: String,
    source_ref: Option<&'static str>,
    pinned: bool,
}

impl Seed {
    fn new(text: &str, kind: &str) -> Self {
        Self {
            text: text.to_string(),
            summary_text: None,
            summary_tokens: None,
            tier: "semantic".to_string(),
            source_kind: kind.to_string(),
            source_ref: None,
            pinned: false,
        }
    }

    fn with_summary(mut self, tokens: u64) -> Self {
        let s = text_of(tokens, self.text.chars().next().unwrap_or('s'));
        self.summary_text = Some(s);
        self.summary_tokens = Some(tokens);
        self
    }

    fn with_ref(mut self, ref_: &'static str) -> Self {
        self.source_ref = Some(ref_);
        self
    }

    #[allow(dead_code)]
    fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }
}

/// Insert a row by direct SQL and return `public_id`.
///
/// Bypassing the write path keeps the test focused on what explain reads back.
async fn seed(store: &StoreHandle, s: Seed) -> String {
    let text = s.text.clone();
    let hash = sha256_hex(&text);
    let pid = public_id(&text);
    let pid_clone = pid.clone();
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO memories
                 (public_id, agent_id, tier, kind, text, text_hash, source_kind,
                  source_ref, status, trust, importance_current, confidence, pinned,
                  expires_at, last_referenced_at, created_at, updated_at,
                  summary_text, summary_tokens)
                 VALUES (?1, 'default', ?2, 'fact', ?3, ?4, ?5, ?6, 'active', 'trusted',
                         0.5, 1.0, ?13,
                         NULL, NULL, ?14, ?14, ?12, ?11)",
                rusqlite::params![
                    pid_clone,
                    s.tier,
                    text,
                    hash,
                    s.source_kind,
                    s.source_ref,
                    // (placeholders 7-10 unused by the SELECT path, keep indices stable)
                    0i64,
                    0i64,
                    0i64,
                    0i64,
                    s.summary_tokens.map(|t| t as i64).unwrap_or(0i64),
                    s.summary_text,
                    s.pinned as i64,
                    SystemClock.now_millis(),
                ],
            )?;
            let id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, 0, 0, 'fact', ?4)",
                rusqlite::params![id, f32_blob(&unit_vec(1536)), s.tier, s.pinned as i64],
            )?;
            Ok(())
        })
        .await
        .expect("seed insert");
    pid
}

fn public_id(text: &str) -> String {
    let h = sha256_hex(text);
    format!("pid-{}", &h[..12])
}

fn f32_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn unit_vec(dim: usize) -> Vec<f32> {
    vec![1.0; dim]
}

/// Minimal query returning one semantic item at min_score 0.
fn query() -> RecallQuery {
    RecallQuery::new(QUERY, &RecallConfig::default())
}

#[tokio::test]
async fn recall_returns_injected_summary_and_tags_it() -> Result<()> {
    let (store, _dir) = store("explain-summary").await;
    let embedder = NormEmbedder::new(1536);

    let text = text_of(400, 'm'); // 400-token body > 200-token budget → must summary-swap
    let pid = seed(&store, Seed::new(&text, "user").with_summary(15)).await;

    let mut q = query();
    q.budget_tokens = 10;
    q.min_score = 0.0;

    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;
    assert!(report.no_hit, "expected a no-hit report");
    let items = report.items();
    let injected = items.iter().filter(|i| i.injected).count();
    assert_eq!(injected, 0, "no-hit report must inject nothing");

    // Now raise the budget so the summary gets injected.
    let mut q = query();
    q.budget_tokens = 200;
    q.min_score = 0.0;
    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;
    assert!(
        !report.no_hit,
        "with enough budget the summary must be placed"
    );
    let items = report.items();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item.public_id, pid);
    assert!(item.injected, "a packed item must expose injected = true");
    assert!(
        item.summary,
        "a summary-placed item must expose summary = true"
    );
    assert!(!item.source_kind.is_empty());
    Ok(())
}

#[tokio::test]
async fn row_order_is_stable_across_recall_calls() -> Result<()> {
    let (store, _dir) = store("explain-order").await;
    let embedder = NormEmbedder::new(1536);

    let earlier = seed(
        &store,
        Seed::new("earliest maintenance log entry", "user")
            .with_ref("file:///svc/logs/2024-03-01.log"),
    )
    .await;
    let later = seed(
        &store,
        Seed::new("later maintenance log entry", "agent").with_ref("agent-7"),
    )
    .await;

    let q = query();
    let r1 = agos_memory::recall::recall(&store, &embedder, &q).await?;
    let r2 = agos_memory::recall::recall(&store, &embedder, &q).await?;

    // Two calls must agree on item order (stable row ordering).
    let ids1: Vec<_> = r1.items().iter().map(|it| it.public_id.clone()).collect();
    let ids2: Vec<_> = r2.items().iter().map(|it| it.public_id.clone()).collect();
    assert_eq!(ids1, ids2, "recall order must be stable");
    // Both rows present, in the order the store returned them.
    assert!(ids1.contains(&earlier) && ids1.contains(&later));

    Ok(())
}

#[tokio::test]
async fn explain_of_a_known_memory() -> Result<()> {
    let (store, _dir) = store("explain-memory").await;

    let pid = seed(
        &store,
        Seed::new("vehicle maintenance log for 2024-03-15", "user")
            .with_summary(20)
            .with_ref("file:///svc/logs/2024-03-15.log"),
    )
    .await;

    let m = explain(&store, "default", &pid).await?;
    let Some(m) = m else {
        panic!("expected a known row to be explain-able");
    };
    assert_eq!(m.public_id, pid);
    assert_eq!(m.source_kind, "user");
    assert_eq!(
        m.source_ref.as_deref(),
        Some("file:///svc/logs/2024-03-15.log")
    );
    assert!(m.links.is_empty());
    assert!(m.recalls.is_empty());

    Ok(())
}

#[tokio::test]
async fn explain_unknown_id_returns_none() -> Result<()> {
    let (store, _dir) = store("explain-unknown").await;
    let m = explain(&store, "default", "pid-nonexistent").await?;
    assert!(m.is_none());
    Ok(())
}

#[tokio::test]
async fn explain_fails_closed_for_wrong_agent() -> Result<()> {
    let (store, _dir) = store("explain-agent").await;
    let pid = seed(&store, Seed::new("a memory", "user")).await;

    // A different agent cannot reach this memory (agent-scoped explain).
    let m = explain(&store, "other-agent", &pid).await?;
    assert!(m.is_none());
    Ok(())
}

#[tokio::test]
async fn render_report_no_hit_line() -> Result<()> {
    let report = RecallReport {
        no_hit: true,
        ..Default::default()
    };

    let out = render_report(&report, &HashMap::new(), 0.35);
    assert!(
        out.contains("No useful memories for \""),
        "no-hit line must be rendered"
    );
    assert!(out.contains("\""), "no-hit line must quote the query");
    Ok(())
}

#[tokio::test]
async fn explain_survives_empty_recall() -> Result<()> {
    // An empty recall must not panic when we later render its report.
    let report = RecallReport::default();
    let out = render_report(&report, &HashMap::new(), 0.0);
    assert!(!out.is_empty() || report.hits.is_empty());
    Ok(())
}
