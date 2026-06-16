//! 0004 acceptance: export → import roundtrip, idempotency, tier filter,
//! conflict versioning, and tombstone refusal.
//!
//! Hermetic end to end: `provider = "none"` (D4) so no network is touched;
//! recall runs BM25-only against the imported store, mirroring
//! `tests/recall_degraded.rs`.

use agos_memory::cli::export::{run_export, run_import};
use agos_memory::config::{Config, EmbedProvider};
use agos_memory::embed::NoEmbedder;
use agos_memory::error::Result;
use agos_memory::recall::{RecallQuery, recall};
use agos_memory::storage::StoreHandle;
use agos_memory::util::{Clock, SystemClock, sha256_hex};
use serde_json::{Value, json};

/// Hermetic config: no embedder, no network (D4 degraded path).
fn hermetic(path: &std::path::Path) -> Config {
    let mut cfg = Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    };
    cfg.embed.provider = EmbedProvider::None;
    cfg
}

async fn open(cfg: &Config) -> StoreHandle {
    StoreHandle::open(cfg, 2).await.expect("open store")
}

/// Seed one active, trusted, semantic-tier row via direct SQL (the pattern
/// from `tests/recall_degraded.rs`) so tests do not depend on the CLI.
async fn seed(store: &StoreHandle, public_id: &str, tier: &str, text: &str) {
    let (pid, tier, text) = (public_id.to_string(), tier.to_string(), text.to_string());
    let hash = sha256_hex(&text);
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO memories
                 (public_id, agent_id, tier, kind, text, text_hash, source_kind,
                  source_ref, status, trust, importance_current, confidence, pinned,
                  expires_at, last_referenced_at, created_at, updated_at,
                  summary_text, summary_tokens)
                 VALUES (?1, 'default', ?2, 'fact', ?3, ?4, 'user',
                         NULL, 'active', 'trusted', 0.5, 1.0, 0,
                         NULL, NULL, ?5, ?5, NULL, NULL)",
                rusqlite::params![pid, tier, text, hash, now],
            )?;
            Ok(())
        })
        .await
        .expect("seed insert");
}

async fn count_memories(store: &StoreHandle) -> i64 {
    store
        .read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
                .map_err(Into::into)
        })
        .await
        .expect("count memories")
}

async fn count_versions(store: &StoreHandle, public_id: &str) -> i64 {
    let pid = public_id.to_string();
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM memory_versions
                 WHERE memory_id = (SELECT id FROM memories WHERE public_id = ?1)",
                rusqlite::params![pid],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
        .await
        .expect("count versions")
}

async fn memory_text(store: &StoreHandle, public_id: &str) -> String {
    let pid = public_id.to_string();
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT text FROM memories WHERE public_id = ?1",
                rusqlite::params![pid],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
        .await
        .expect("read text")
}

async fn memory_id(store: &StoreHandle, public_id: &str) -> i64 {
    let pid = public_id.to_string();
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT id FROM memories WHERE public_id = ?1",
                rusqlite::params![pid],
                |r| r.get(0),
            )
            .map_err(Into::into)
        })
        .await
        .expect("memory id")
}

// -----------------------------------------------------------------------
// AC 1: export → import into a fresh store → recall finds everything.
// -----------------------------------------------------------------------

#[tokio::test]
async fn export_import_roundtrip_is_recallable() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = hermetic(&dir.path().join("src.db"));
    let store = open(&cfg).await;
    seed(
        &store,
        "pid-round-a",
        "semantic",
        "vehicle maintenance log for the north fleet",
    )
    .await;
    seed(
        &store,
        "pid-round-b",
        "semantic",
        "vehicle maintenance log audit concluded for q4",
    )
    .await;
    drop(store);

    let out = dir.path().join("export.jsonl");
    run_export(&cfg, None, Some(out.as_path())).await?;

    // Header contract: first line carries the format marker.
    let raw = std::fs::read_to_string(&out).expect("export file readable");
    let lines: Vec<&str> = raw.lines().collect();
    assert!(lines.len() >= 3, "header + 2 rows expected: {lines:?}");
    let header: Value = serde_json::from_str(lines[0]).expect("header is JSON");
    assert!(header.get("format").is_some(), "header: {header}");

    // Import into a fresh store and prove recall sees both rows (BM25).
    let cfg_b = hermetic(&dir.path().join("dst.db"));
    run_import(&cfg_b, out.as_path(), false).await?;
    let store_b = open(&cfg_b).await;
    assert_eq!(count_memories(&store_b).await, 2, "both rows imported");

    let query = RecallQuery::new("vehicle maintenance log", &cfg_b.recall);
    let report = recall(&store_b, &NoEmbedder, &query).await?;
    assert!(!report.no_hit, "imported memories must be recallable");
    assert!(
        report.hits.len() >= 2,
        "both imported rows should rank in BM25 results"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// AC 2: dry-run writes nothing; re-importing the same file is a no-op.
// -----------------------------------------------------------------------

#[tokio::test]
async fn reimport_is_idempotent_and_dry_run_writes_nothing() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = hermetic(&dir.path().join("src.db"));
    let store = open(&cfg).await;
    seed(
        &store,
        "pid-idem-1",
        "semantic",
        "recalled idempotency fixture one",
    )
    .await;
    seed(
        &store,
        "pid-idem-2",
        "semantic",
        "recalled idempotency fixture two",
    )
    .await;
    seed(
        &store,
        "pid-idem-3",
        "semantic",
        "recalled idempotency fixture three",
    )
    .await;
    drop(store);

    let out = dir.path().join("export.jsonl");
    run_export(&cfg, None, Some(out.as_path())).await?;

    let cfg_b = hermetic(&dir.path().join("dst.db"));

    // Dry run: validates but must not write.
    run_import(&cfg_b, out.as_path(), true).await?;
    let sb = open(&cfg_b).await;
    assert_eq!(count_memories(&sb).await, 0, "dry-run must write nothing");
    drop(sb);

    // First real import inserts all rows.
    run_import(&cfg_b, out.as_path(), false).await?;
    let sb = open(&cfg_b).await;
    assert_eq!(count_memories(&sb).await, 3, "first import inserts all");
    drop(sb);

    // Replay: hash dedup keeps row counts stable — no duplicates, ever.
    run_import(&cfg_b, out.as_path(), false).await?;
    let sb = open(&cfg_b).await;
    assert_eq!(
        count_memories(&sb).await,
        3,
        "re-import must be idempotent (no duplicate rows)"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// AC 3: `export --tier X` keeps only rows of that tier.
// -----------------------------------------------------------------------

#[tokio::test]
async fn tier_filter_exports_only_that_tier() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = hermetic(&dir.path().join("src.db"));
    let store = open(&cfg).await;
    seed(
        &store,
        "pid-tier-sem",
        "semantic",
        "tier filter semantic row",
    )
    .await;
    seed(
        &store,
        "pid-tier-proc",
        "procedural",
        "tier filter procedural row",
    )
    .await;
    drop(store);

    let out = dir.path().join("semantic.jsonl");
    run_export(&cfg, Some("semantic"), Some(out.as_path())).await?;

    let raw = std::fs::read_to_string(&out).expect("export file readable");
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 2, "header + exactly one semantic row");
    let row: Value = serde_json::from_str(lines[1]).expect("row is JSON");
    assert_eq!(row["tier"].as_str(), Some("semantic"), "row: {row}");

    // And the filtered file imports as exactly one memory.
    let cfg_b = hermetic(&dir.path().join("dst.db"));
    run_import(&cfg_b, out.as_path(), false).await?;
    let sb = open(&cfg_b).await;
    assert_eq!(
        count_memories(&sb).await,
        1,
        "only the semantic row imports"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// AC 4: same public_id + different text_hash → new version row, never a
// silent overwrite and never a duplicate row.
// -----------------------------------------------------------------------

#[tokio::test]
async fn conflicting_import_creates_a_version_row() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = hermetic(&dir.path().join("src.db"));
    let store = open(&cfg).await;
    seed(
        &store,
        "pid-conflict",
        "semantic",
        "fleet policy original wording",
    )
    .await;
    drop(store);

    let out = dir.path().join("export.jsonl");
    run_export(&cfg, None, Some(out.as_path())).await?;

    let cfg_b = hermetic(&dir.path().join("dst.db"));
    run_import(&cfg_b, out.as_path(), false).await?;
    let sb = open(&cfg_b).await;
    let versions_before = count_versions(&sb, "pid-conflict").await;
    drop(sb);

    // Rewrite the exported row: same public_id, new text + matching hash.
    let raw = std::fs::read_to_string(&out).expect("export file readable");
    let lines: Vec<&str> = raw.lines().collect();
    let new_text = "fleet policy rewritten wording after review";
    let mut rewritten: Vec<String> = vec![lines[0].to_string()];
    let mut touched = false;
    for line in &lines[1..] {
        let mut row: Value = serde_json::from_str(line).expect("row is JSON");
        if row["public_id"].as_str() == Some("pid-conflict") {
            row["text"] = json!(new_text);
            row["text_hash"] = json!(sha256_hex(new_text));
            touched = true;
        }
        rewritten.push(row.to_string());
    }
    assert!(touched, "fixture row not found in export");
    let out2 = dir.path().join("conflict.jsonl");
    std::fs::write(&out2, rewritten.join("\n") + "\n").unwrap();

    run_import(&cfg_b, out2.as_path(), false).await?;

    let sb = open(&cfg_b).await;
    assert_eq!(
        count_memories(&sb).await,
        1,
        "conflict must not duplicate the row"
    );
    assert_eq!(
        memory_text(&sb, "pid-conflict").await,
        new_text,
        "conflicting text becomes current"
    );
    let versions_after = count_versions(&sb, "pid-conflict").await;
    assert!(
        versions_after > versions_before,
        "conflict must append a version row ({versions_before} -> {versions_after})"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// AC 5: a stale export containing a tombstoned public_id is refused and
// nothing is resurrected.
// -----------------------------------------------------------------------

#[tokio::test]
async fn import_refuses_tombstoned_public_ids() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = hermetic(&dir.path().join("src.db"));
    let store = open(&cfg).await;
    seed(
        &store,
        "pid-tomb",
        "semantic",
        "privacy sensitive row bound for hard purge",
    )
    .await;
    // Release the process lock before export opens its own handle (D18).
    drop(store);

    // Export while the row still exists — a stale backup file.
    let out = dir.path().join("stale.jsonl");
    run_export(&cfg, None, Some(out.as_path())).await?;

    // Hard-purge it: row deleted, tombstone appended (D31).
    let store = open(&cfg).await;
    let id = memory_id(&store, "pid-tomb").await;
    store
        .hard_purge_memory(id, None, Some("acceptance test"))
        .await?;
    drop(store);

    let err = run_import(&cfg, out.as_path(), false).await;
    assert!(
        err.is_err(),
        "import must refuse a tombstoned public_id, not resurrect it"
    );

    let store = open(&cfg).await;
    assert_eq!(
        count_memories(&store).await,
        0,
        "refused import must not write anything"
    );
    Ok(())
}
