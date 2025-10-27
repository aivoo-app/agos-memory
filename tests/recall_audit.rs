//! 0036 acceptance: the audit trail writes `recalls`, `recall_items`, and
//! `token_ledger` rows with the correct counts and round-tripable content.
//!
//! Coverage:
//! - After `recall()`, `recalls` has exactly 1 row with correct counts.
//! - `recall_items` has one row per injected hit, with correct rank/score.
//! - `components_json` round-trips through serde.
//! - `token_ledger` has 1 row with correct tier split.
//! - `explain(id)` returns a memory that was injected into a recall.
//! - `no_hit` recall still writes the audit row.

mod common;

use agos_memory::config::RecallConfig;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, explain};
use agos_memory::storage::StoreHandle;
use agos_memory::util::SystemClock;
use agos_memory::util::clock::Clock;
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
    let now = SystemClock.now_millis();
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
            let blob: Vec<u8> = (0..1536).flat_map(|_| 1.0f32.to_le_bytes()).collect();
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
async fn recall_audit_writes_all_three_rows() -> Result<()> {
    let (store, _dir) = store("audit-rows").await;
    let embedder = NormEmbedder::new(1536);

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let mut q = query();
    q.budget_tokens = 200;
    q.min_score = 0.0;
    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;
    assert!(!report.no_hit);
    assert_eq!(report.hits.len(), 1);

    // 1. recalls row
    let row: (i64, i64, i64, i64, i64, i64, Option<f64>, i64) = store
        .read(|conn| {
            conn.query_row(
                "SELECT id, k, budget, candidates, injected, dropped, top_score, no_hit
                 FROM recalls ORDER BY id DESC LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .map_err(Into::into)
        })
        .await?;
    assert_eq!(row.0, 1, "one recall row");
    assert_eq!(row.1, 8, "k = top_k default");
    assert_eq!(row.2, 200, "budget matched");
    assert_eq!(row.4, 1, "injected = 1");
    assert_eq!(row.5, 0, "dropped = 0");
    assert!(row.6.is_some(), "top_score present");
    assert_eq!(row.7, 0, "no_hit = 0");

    // 2. recall_items rows — one per injected hit
    let (count, score, rank): (i64, f64, i64) = store
        .read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*), score, rank FROM recall_items WHERE recall_id = ?1",
                [row.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(Into::into)
        })
        .await?;
    assert_eq!(count, 1, "one recall_items row per hit");
    assert_eq!(rank, 0, "rank = 0 for the single hit");
    assert!(
        (score - report.hits[0].score).abs() < 1e-9,
        "score round-trips"
    );

    // 3. token_ledger row
    let (_tl_count, budget, _tokens, items_inj, items_drop): (i64, i64, i64, i64, i64) = store
        .read(|conn| {
            conn.query_row(
                "SELECT COUNT(*), budget, tokens_used, items_injected, items_dropped
                     FROM token_ledger ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .map_err(Into::into)
        })
        .await?;
    assert_eq!(budget, 200, "budget matched");
    assert_eq!(items_inj, 1, "items_injected matched");
    assert_eq!(items_drop, 0, "items_dropped matched");

    Ok(())
}

#[tokio::test]
async fn components_json_round_trips() -> Result<()> {
    let (store, _dir) = store("audit-components").await;
    let embedder = NormEmbedder::new(1536);

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let mut q = query();
    q.budget_tokens = 200;
    q.min_score = 0.0;
    let _report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    // Read back the components_json and deserialize it.
    let json: String = store
        .read(|conn| {
            conn.query_row(
                "SELECT components_json FROM recall_items ORDER BY id LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .map_err(Into::into)
        })
        .await?;
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    assert!(parsed.is_object(), "components_json is a JSON object");
    assert!(parsed.get("rrf").is_some(), "components_json has rrf field");

    Ok(())
}

#[tokio::test]
async fn tier_split_json_round_trips() -> Result<()> {
    let (store, _dir) = store("audit-tier-split").await;
    let embedder = NormEmbedder::new(1536);

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let mut q = query();
    q.budget_tokens = 200;
    q.min_score = 0.0;
    let _report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    let json: String = store
        .read(|conn| {
            conn.query_row(
                "SELECT tier_split_json FROM token_ledger ORDER BY id LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .map_err(Into::into)
        })
        .await?;
    let parsed: serde_json::Value = serde_json::from_str(&json)?;
    assert!(parsed.is_object(), "tier_split_json is a JSON object");
    assert!(
        parsed.get("working").is_some(),
        "tier_split_json has working field"
    );
    assert!(parsed.get("semantic").is_some());

    Ok(())
}

#[tokio::test]
async fn no_hit_recall_still_writes_audit() -> Result<()> {
    let (store, _dir) = store("audit-no-hit").await;
    let embedder = NormEmbedder::new(1536);

    // Seed nothing relevant — query will return no hits.
    let mut q = query();
    q.min_score = 0.99; // nothing will match
    q.budget_tokens = 50;
    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;
    assert!(report.no_hit);

    // Audit row must still exist
    let count: i64 = store
        .read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM recalls", [], |r| r.get(0))
                .map_err(Into::into)
        })
        .await?;
    assert_eq!(count, 1, "no-hit recall must still write audit row");

    // And the no_hit flag must be set
    let no_hit: i64 = store
        .read(|conn| {
            conn.query_row(
                "SELECT no_hit FROM recalls ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Into::into)
        })
        .await?;
    assert_eq!(no_hit, 1, "no_hit flag = 1");

    Ok(())
}

#[tokio::test]
async fn explain_returns_recall_injections() -> Result<()> {
    let (store, _dir) = store("audit-explain-injections").await;
    let embedder = NormEmbedder::new(1536);

    let pid = seed(&store, "vehicle maintenance log for 2024").await;

    let mut q = query();
    q.budget_tokens = 200;
    q.min_score = 0.0;
    let _report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    // Now explain the memory — it should show the recall that injected it.
    let m = explain(&store, "default", &pid).await?;
    let Some(m) = m else {
        panic!("expected a known row to be explain-able after recall");
    };
    assert_eq!(m.public_id, pid);
    assert!(
        !m.recalls.is_empty(),
        "explain must show recall injections after a recall call"
    );
    // The injection should be from the most recent recall
    let (recall_id, rank, _score, injected) = m.recalls[0];
    assert!(recall_id > 0, "recall_id positive");
    assert_eq!(rank, 0, "rank = 0 for the single injected memory");
    assert!(injected, "must be marked injected");

    Ok(())
}

#[tokio::test]
async fn dropped_items_still_get_audit_rows() -> Result<()> {
    let (store, _dir) = store("audit-dropped").await;
    let embedder = NormEmbedder::new(1536);

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    // Tiny budget → the item gets dropped whole (its text > budget).
    let mut q = query();
    q.budget_tokens = 1;
    q.min_score = 0.0;
    let report = agos_memory::recall::recall(&store, &embedder, &q).await?;

    // The hit is in the report but dropped (kept for audit trail).
    assert!(!report.hits.is_empty(), "hit is retained as dropped");
    let injected = report.hits.iter().filter(|h| h.injected()).count();
    assert_eq!(injected, 0, "nothing injected with 1-token budget");

    // Audit row must record the dropped item.
    let dropped: i64 = store
        .read(|conn| {
            conn.query_row(
                "SELECT dropped FROM recalls ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(Into::into)
        })
        .await?;
    assert!(dropped > 0, "dropped items counted in audit");

    // And recall_items rows exist for dropped hits too.
    let item_count: i64 = store
        .read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM recall_items", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(Into::into)
        })
        .await?;
    assert!(item_count > 0, "dropped items get recall_items rows");

    Ok(())
}

#[tokio::test]
async fn audit_is_idempotent_across_calls() -> Result<()> {
    let (store, _dir) = store("audit-multi").await;
    let embedder = NormEmbedder::new(1536);

    let _pid = seed(&store, "vehicle maintenance log for 2024").await;

    let q = query();
    let _ = agos_memory::recall::recall(&store, &embedder, &q).await?;
    let _ = agos_memory::recall::recall(&store, &embedder, &q).await?;
    let _ = agos_memory::recall::recall(&store, &embedder, &q).await?;

    let count: i64 = store
        .read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM recalls", [], |r| r.get::<_, i64>(0))
                .map_err(Into::into)
        })
        .await?;
    assert_eq!(count, 3, "each recall call writes one audit row");

    Ok(())
}
