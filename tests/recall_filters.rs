//! Hard-filter acceptance tests (issue 0032) — the "zero leaks" claim.
//!
//! Two properties are proven here:
//!
//! 1. **One predicate, two forms, no drift.** [`HardFilter::predicate`] (SQL)
//!    and [`HardFilter::admits`] (Rust mirror) select the *same* rows, checked
//!    row-for-row against an independently written expectation table. Agreement
//!    alone would not be enough — both could be wrong the same way — so the
//!    expected set is spelled out per seeded row.
//! 2. **Zero leaks on both retrieval paths.** Rows that must never surface are
//!    seeded to be *easy* to find: they share the query's rare keyword, their
//!    vectors sit in the search region, and they are seeded by direct SQL so no
//!    write-path behaviour can mask a filter gap. The same query is run with a
//!    working embedder (vec + FTS) and a refusing one (FTS only).

mod common;

use common::{DIM, NormEmbedder, store};

use agos_memory::config::{Config, TrustPolicy};
use agos_memory::error::Result;
use agos_memory::recall::{HardFilter, RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{Clock, SystemClock, sha256_hex};

/// Rare token every seeded row contains, so the FTS leg wants them all.
const MAGIC: &str = "zanzibar";

/// Public id derived from the text, so seeding is deterministic.
fn public_id(text: &str) -> String {
    format!("pub_{}", &sha256_hex(text)[..12])
}

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Fields the hard filter reads, spelled out so each test states its intent.
struct Row {
    agent_id: String,
    text: String,
    tier: String,
    status: String,
    trust: String,
    expires_at: Option<i64>,
    last_referenced_at: Option<i64>,
    created_at: i64,
}

impl Row {
    /// An admitted row: own agent, in-tier, active, trusted, no expiry.
    fn admitted(text: &str, created_at: i64) -> Self {
        Self {
            agent_id: "default".to_string(),
            text: text.to_string(),
            tier: "semantic".to_string(),
            status: "active".to_string(),
            trust: "trusted".to_string(),
            expires_at: None,
            last_referenced_at: None,
            created_at,
        }
    }

    fn agent(mut self, v: &str) -> Self {
        self.agent_id = v.to_string();
        self
    }

    fn tier(mut self, v: &str) -> Self {
        self.tier = v.to_string();
        self
    }

    fn status(mut self, v: &str) -> Self {
        self.status = v.to_string();
        self
    }

    fn trust(mut self, v: &str) -> Self {
        self.trust = v.to_string();
        self
    }

    fn expires_at(mut self, v: i64) -> Self {
        self.expires_at = Some(v);
        self
    }

    fn last_referenced_at(mut self, v: i64) -> Self {
        self.last_referenced_at = Some(v);
        self
    }
}

/// Insert a row by direct SQL (plus its vec0 row) and return its rowid.
///
/// Bypassing the write path is deliberate: the hard filter must hold against
/// rows it did not create, including ones with combinations the writer would
/// never produce.
async fn seed(store: &StoreHandle, row: Row) -> i64 {
    let text = row.text.clone();
    let hash = sha256_hex(&text);
    let pid = public_id(&text);
    let pid_for_write = pid.clone();
    let vec = blob(&vec![0.25f32; DIM]);
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO memories (public_id, agent_id, tier, kind, text, text_hash,
                 status, trust, expires_at, last_referenced_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'fact', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                rusqlite::params![
                    pid_for_write,
                    row.agent_id,
                    row.tier,
                    text,
                    hash,
                    row.status,
                    row.trust,
                    row.expires_at,
                    row.last_referenced_at,
                    row.created_at,
                ],
            )?;
            let id = conn.last_insert_rowid();
            let status_code = match row.status.as_str() {
                "active" => 0,
                "pending" => 1,
                "deprecated" => 2,
                _ => 3,
            };
            let trust_code = match row.trust.as_str() {
                "trusted" => 0,
                "untrusted" => 1,
                _ => 2,
            };
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'fact', 0)",
                rusqlite::params![id, vec, row.tier, status_code, trust_code],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    // The rowid is stable and deterministic to re-read.
    store
        .read(move |conn| {
            let id: i64 = conn.query_row(
                "SELECT id FROM memories WHERE public_id = ?1",
                rusqlite::params![pid],
                |r| r.get(0),
            )?;
            Ok(id)
        })
        .await
        .unwrap()
}

/// Mark `old` as replaced by `new` the way the write path does.
async fn supersede_backfill(store: &StoreHandle, old: i64, new: i64) {
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET superseded_by_id = ?1 WHERE id = ?2",
                rusqlite::params![new, old],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Mark `old` as replaced without back-filling `superseded_by_id` (the
/// hardening case: a writer that only sets `supersedes_id` on the new row).
async fn supersede_forward_only(store: &StoreHandle, old: i64, new: i64) {
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET supersedes_id = ?1 WHERE id = ?2",
                rusqlite::params![old, new],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

fn query(text: &str, k: usize) -> RecallQuery {
    let mut q = RecallQuery::new(text, &Config::default().recall);
    q.k = k;
    q
}

/// An embedder that always refuses, so recall falls back to the FTS leg alone.
struct FailingEmbedder;

#[async_trait::async_trait]
impl agos_memory::embed::Embedder for FailingEmbedder {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(agos_memory::error::Error::Embedder("provider down".into()))
    }

    fn model(&self) -> &str {
        "failing-mock"
    }

    fn dim(&self) -> usize {
        DIM
    }
}

/// Total rows in `memories` — proves the forbidden rows really are in the DB,
/// so their absence from results is a filter effect and not a missing fixture.
async fn db_total(store: &StoreHandle) -> i64 {
    store
        .read(|conn| {
            let n: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
            Ok(n)
        })
        .await
        .unwrap()
}

/// Seed one row of every hard-filter category and return
/// `(admitted texts, forbidden texts)`.
async fn seed_matrix(store: &StoreHandle, now: i64) -> (Vec<String>, Vec<String>) {
    let t = |name: &str| format!("zanzibar {name}");

    let _ok_semantic = seed(store, Row::admitted(&t("keep semantic"), now - 1_000)).await;
    let _ok_working = seed(
        store,
        Row::admitted(&t("keep working"), now - 1_000)
            .tier("working")
            .trust("system"),
    )
    .await;
    let replacement = seed(store, Row::admitted(&t("keep replacement"), now - 1_000)).await;

    // Forbidden, one category each.
    seed(
        store,
        Row::admitted(&t("foreign agent"), now - 1_000).agent("intruder"),
    )
    .await;
    seed(
        store,
        Row::admitted(&t("deprecated"), now - 1_000).status("deprecated"),
    )
    .await;
    seed(
        store,
        Row::admitted(&t("deleted"), now - 1_000).status("deleted"),
    )
    .await;
    seed(
        store,
        Row::admitted(&t("pending"), now - 1_000).status("pending"),
    )
    .await;
    seed(
        store,
        Row::admitted(&t("untrusted"), now - 1_000).trust("untrusted"),
    )
    .await;
    seed(
        store,
        Row::admitted(&t("expired"), now - 1_000).expires_at(now - 60_000),
    )
    .await;
    let superseded_old = seed(store, Row::admitted(&t("superseded old"), now - 1_000)).await;
    let forward_only_old = seed(store, Row::admitted(&t("forward only old"), now - 1_000)).await;
    seed(
        store,
        Row::admitted(&t("episodic"), now - 1_000).tier("episodic"),
    )
    .await;
    seed(
        store,
        Row::admitted(&t("clock skew"), now - 1_000).last_referenced_at(now - 30_000),
    )
    .await;

    // Supersede direction: back-filled (old carries superseded_by_id) and
    // forward-only (only the replacement carries supersedes_id).
    supersede_backfill(store, superseded_old, replacement).await;
    supersede_forward_only(store, forward_only_old, replacement).await;

    let admitted = vec![t("keep semantic"), t("keep working"), t("keep replacement")];
    let forbidden = vec![
        t("foreign agent"),
        t("deprecated"),
        t("deleted"),
        t("pending"),
        t("untrusted"),
        t("expired"),
        t("superseded old"),
        t("forward only old"),
        t("episodic"),
        t("clock skew"),
    ];
    (admitted, forbidden)
}

/// A filter with the default policy, pinned to `now`.
fn filter_at(now: i64) -> HardFilter {
    HardFilter::from_query("default", &query(MAGIC, 50), now)
}

/// SQL gate: public ids the predicate selects.
async fn sql_selected(store: &StoreHandle, filter: &HardFilter) -> Vec<String> {
    let sql = format!(
        "SELECT m.public_id FROM memories m WHERE {}",
        filter.predicate("m")
    );
    let mut ids = store
        .read(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
        .unwrap();
    ids.sort();
    ids
}

/// Rust gate: public ids the mirror admits, over the same rows.
async fn rust_selected(store: &StoreHandle, filter: &HardFilter) -> Vec<String> {
    let mut admitted = store
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.public_id, m.agent_id, m.tier, m.status, m.trust,
                        m.expires_at, m.superseded_by_id,
                        (SELECT s.id FROM memories s WHERE s.supersedes_id = m.id LIMIT 1),
                        m.created_at, m.last_referenced_at,
                        m.importance_current, m.confidence
                 FROM memories m",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(agos_memory::recall::CanonicalRow {
                    rowid: r.get(0)?,
                    public_id: r.get(1)?,
                    agent_id: r.get(2)?,
                    tier: r.get(3)?,
                    status: r.get(4)?,
                    trust: r.get(5)?,
                    expires_at: r.get(6)?,
                    superseded_by_id: r.get(7)?,
                    successor_id: r.get(8)?,
                    created_at: r.get(9)?,
                    last_referenced_at: r.get(10)?,
                    // Not read by the predicate; the rerank terms (0033) are
                    // proven separately in `tests/recall_rerank.rs`.
                    importance_current: r.get(11)?,
                    confidence: r.get(12)?,
                    // Packing inputs (0034): not read by SQL or the mirror.
                    pinned: false,
                    text: String::new(),
                    summary_text: None,
                    summary_tokens: None,
                    // Provenance (0035): not read by the predicate or packing.
                    source_kind: "user".into(),
                    source_ref: None,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
        .unwrap()
        .into_iter()
        .filter(|r| filter.admits(r))
        .map(|r| r.public_id)
        .collect::<Vec<_>>();
    admitted.sort();
    admitted
}

/// Pin a row, to show `pinned` does not launder provenance.
async fn pin(store: &StoreHandle, pid: String) {
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE memories SET pinned = 1 WHERE public_id = ?1",
                rusqlite::params![pid],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

/// Sorted public ids of a report's hits.
fn hit_ids(report: &agos_memory::recall::RecallReport) -> Vec<String> {
    let mut ids = report
        .hits
        .iter()
        .map(|h| h.public_id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

/// Sorted public ids for a list of seeded texts.
fn expected_ids(texts: &[String]) -> Vec<String> {
    let mut ids = texts.iter().map(|t| public_id(t)).collect::<Vec<_>>();
    ids.sort();
    ids
}

#[tokio::test]
async fn predicate_and_mirror_agree_row_for_row() {
    let (store, _dir) = store("filter_equiv").await;
    let now = SystemClock.now_millis();
    let (admitted, forbidden) = seed_matrix(&store, now).await;
    let filter = filter_at(now);

    let sql = sql_selected(&store, &filter).await;
    let rust = rust_selected(&store, &filter).await;
    let expected = expected_ids(&admitted);

    // Membership first (an expectation table, so agreement cannot hide a
    // shared mistake), then the two forms against each other.
    assert_eq!(
        sql, expected,
        "SQL predicate must select exactly the admitted rows"
    );
    assert_eq!(
        rust, expected,
        "Rust mirror must admit exactly the admitted rows"
    );
    assert_eq!(sql, rust, "SQL and Rust forms must not drift apart");

    // Non-vacuous: every forbidden row is really in the DB, so its absence
    // above is a filter decision and not a missing fixture.
    assert_eq!(
        db_total(&store).await,
        (admitted.len() + forbidden.len()) as i64,
        "fixture must be fully seeded"
    );
    for text in &forbidden {
        let pid = public_id(text);
        assert!(!sql.contains(&pid), "SQL gate leaked {text}");
        assert!(!rust.contains(&pid), "Rust gate leaked {text}");
    }
}

#[tokio::test]
async fn zero_leaks_on_both_retrieval_paths() {
    let (store, _dir) = store("filter_leaks").await;
    let now = SystemClock.now_millis();
    let (admitted, forbidden) = seed_matrix(&store, now).await;
    let expected = expected_ids(&admitted);

    // Both legs live: k=50 covers every row, and every row contains the
    // query's rare token, so both legs actively want the forbidden rows.
    let emb = NormEmbedder::new(DIM);
    let hybrid = recall(&store, &emb, &query(MAGIC, 50)).await.unwrap();
    assert!(!hybrid.degraded, "vec leg must be live for this path");
    assert_eq!(
        hit_ids(&hybrid),
        expected,
        "hybrid recall must return exactly the admitted rows"
    );

    // FTS-only path: same guarantee with the vec leg out of the picture.
    let fts_only = recall(&store, &FailingEmbedder, &query(MAGIC, 50))
        .await
        .unwrap();
    assert!(
        fts_only.degraded,
        "refusing embedder must degrade, not fail"
    );
    assert_eq!(
        hit_ids(&fts_only),
        expected,
        "FTS-only recall must return exactly the admitted rows"
    );

    for text in &forbidden {
        let pid = public_id(text);
        assert!(
            !hit_ids(&hybrid).contains(&pid),
            "hybrid path leaked {text}"
        );
        assert!(
            !hit_ids(&fts_only).contains(&pid),
            "FTS-only path leaked {text}"
        );
    }
}

#[tokio::test]
async fn replacement_row_wins_over_the_row_it_replaces() {
    let (store, _dir) = store("filter_supersede").await;
    let now = SystemClock.now_millis();
    let (_admitted, forbidden) = seed_matrix(&store, now).await;
    let emb = NormEmbedder::new(DIM);

    let report = recall(&store, &emb, &query(MAGIC, 50)).await.unwrap();
    let ids = hit_ids(&report);

    assert!(
        ids.contains(&public_id("zanzibar keep replacement")),
        "the replacement row must be retrievable"
    );
    for old in ["zanzibar superseded old", "zanzibar forward only old"] {
        assert!(
            forbidden.contains(&old.to_string()),
            "fixture must contain the replaced row"
        );
        assert!(!ids.contains(&public_id(old)), "replaced row leaked: {old}");
    }
}

#[tokio::test]
async fn untrusted_is_fenced_only_when_opted_in_and_never_pinned_through() {
    let (store, _dir) = store("filter_untrusted").await;
    let now = SystemClock.now_millis();
    let trusted_text = "zanzibar trusted note".to_string();
    let untrusted_text = "zanzibar untrusted note".to_string();
    let trusted = public_id(&trusted_text);
    let untrusted = public_id(&untrusted_text);

    // Seeded by direct SQL, so trust comes only from the row itself.
    seed(&store, Row::admitted(&trusted_text, now - 1_000)).await;
    seed(
        &store,
        Row::admitted(&untrusted_text, now - 1_000).trust("untrusted"),
    )
    .await;
    // A pinned untrusted row is still untrusted (D29 precedes pinning).
    pin(&store, untrusted.clone()).await;

    let emb = NormEmbedder::new(DIM);

    // Strict (default): the untrusted row is invisible on both legs.
    let strict = recall(&store, &emb, &query(MAGIC, 50)).await.unwrap();
    let ids = hit_ids(&strict);
    assert!(!ids.contains(&untrusted), "Strict must exclude untrusted");
    assert!(ids.contains(&trusted), "Strict must keep trusted");

    // Fenced: it surfaces, explicitly marked so the caller can fence it.
    let mut fenced_q = query(MAGIC, 50);
    fenced_q.trust_policy = TrustPolicy::Fenced;
    let fenced = recall(&store, &emb, &fenced_q).await.unwrap();
    let hit = fenced
        .hits
        .iter()
        .find(|h| h.public_id == untrusted)
        .expect("Fenced must surface the untrusted row");
    assert_eq!(
        hit.trust, "untrusted",
        "the marker is what lets the caller fence it"
    );
    assert!(hit_ids(&fenced).contains(&trusted));
}
