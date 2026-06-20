//! Issue 0001 (v0.6.0) — §10 forgotten-item leak proof.
//!
//! The same bait is written through the public API, then verified through every
//! product surface named by the roadmap: recall, JSONL export, and a fresh
//! backup snapshot. The backup taken *before* the purge is a required negative
//! control: snapshots cannot retroactively forget, so it must still contain the
//! bait and the runbook must tell operators how to handle that risk.
//!
//! SQL checks discover every application table dynamically, including FTS5 and
//! sqlite-vec shadow tables. Raw-byte checks catch deleted payload left in free
//! pages that an ordinary query cannot see.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agos_memory::api::{ForgetInput, MemoryApi, RecallInput, RememberInput};
use agos_memory::cli::export::export;
use agos_memory::config::{Config, EmbedProvider};
use agos_memory::embed::{Embedder, HashEmbedder};
use agos_memory::error::Result;
use agos_memory::llm::{ChatClient, MockChat};
use agos_memory::storage::StoreHandle;

use rusqlite::Connection;

const BAIT: &str = "ZQX7WV9F3K";
const BAIT_TEXT: &str = "The emergency codeword ZQX7WV9F3K opens the north vault.";
const RECALL_QUERY: &str = "emergency codeword";

fn bait_in(bytes: &[u8]) -> bool {
    bytes
        .windows(BAIT.len())
        .any(|window| window == BAIT.as_bytes())
}

fn file_has_bait(path: &Path) -> bool {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("cannot read {} for leak scan: {e}", path.display()));
    bait_in(&bytes)
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut path = db.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn table_names(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Return every `table.column (count)` whose TEXT/BLOB value contains `bait`.
fn survivor_columns(conn: &Connection, bait: &str) -> Result<Vec<String>> {
    let mut survivors = Vec::new();

    for table in table_names(conn)? {
        let quoted_table = quote_ident(&table);
        let columns = {
            let mut stmt = conn.prepare(&format!("PRAGMA table_info({quoted_table})"))?;
            stmt.query_map([], |row| {
                let name: String = row.get(1)?;
                let kind: String = row.get(2)?;
                Ok((name, kind))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
        };

        for (column, kind) in columns {
            if !kind.eq_ignore_ascii_case("text") && !kind.eq_ignore_ascii_case("blob") {
                continue;
            }
            let quoted_column = quote_ident(&column);
            let sql = format!(
                "SELECT COUNT(*) FROM {quoted_table}
                 WHERE instr(CAST({quoted_column} AS TEXT), ?1) > 0"
            );
            let count: i64 = conn.query_row(&sql, [bait], |row| row.get(0))?;
            if count > 0 {
                survivors.push(format!("{table}.{column} ({count})"));
            }
        }
    }

    survivors.sort();
    Ok(survivors)
}

fn snapshot_survivors(path: &Path, bait: &str) -> Result<Vec<String>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    survivor_columns(&conn, bait)
}

/// Ask FTS5 itself whether the bait is indexed. Shadow BLOB values are
/// tokenized, so `CAST(blob AS TEXT)` is not a valid FTS survival probe.
fn fts_match_count(conn: &Connection, bait: &str) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM fts_memories WHERE fts_memories MATCH ?1",
        [bait],
        |row| row.get(0),
    )?)
}

async fn make_api() -> (MemoryApi, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("leak.db");
    let cfg = Config {
        db_path: db_path.clone(),
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Default::default()
    };
    let store = StoreHandle::open(&cfg, 2).await.expect("open store");
    let dim = store.embed_dim().await.expect("embed dimension");
    let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(dim));
    let chat: Arc<dyn ChatClient> = Arc::new(MockChat::default());
    (MemoryApi::new(store, cfg, embedder, chat), db_path, dir)
}

async fn recall_public_ids(api: &MemoryApi) -> Result<Vec<String>> {
    let report = api
        .recall(&RecallInput {
            text: RECALL_QUERY.into(),
            k: Some(8),
            budget_tokens: None,
            include_untrusted: Some(true),
            include_pending: Some(true),
            include_episodic: Some(true),
        })
        .await?;
    Ok(report.hits.into_iter().map(|hit| hit.public_id).collect())
}

#[tokio::test]
async fn hard_purge_is_unreachable_through_recall_backup_and_export() -> Result<()> {
    let (api, db_path, dir) = make_api().await;
    let store = api.store();

    // Positive control: the public write path populated recall, and the bait is
    // returned before the purge. The query itself deliberately omits the bait,
    // so recall audit rows are not expected to contain it.
    let remembered = api
        .remember(&RememberInput {
            text: BAIT_TEXT.into(),
            tier: Some("episodic".into()),
            kind: Some("fact".into()),
            source_kind: Some("agent".into()),
            confidence: Some(1.0),
        })
        .await?;
    let public_id = remembered.public_id;
    assert!(recall_public_ids(&api).await?.contains(&public_id));

    // Positive controls for the other two product surfaces.
    let export_before = dir.path().join("before.jsonl");
    export(store, None, Some(&export_before)).await?;
    let before_export = std::fs::read_to_string(&export_before)
        .unwrap_or_else(|e| panic!("cannot read pre-purge export: {e}"));
    assert!(
        before_export.contains(BAIT),
        "pre-purge export lost its bait"
    );
    assert!(
        before_export.contains(&public_id),
        "pre-purge export lost its id"
    );

    let snapshot_before = dir.path().join("before.db");
    let snapshot_report = store.snapshot_to(&snapshot_before).await?;
    assert_eq!(snapshot_report.integrity, "ok");
    assert!(
        file_has_bait(&snapshot_before),
        "pre-purge snapshot must retain the bait (negative control)"
    );
    let before_survivors = snapshot_survivors(&snapshot_before, BAIT)?;
    assert!(
        before_survivors
            .iter()
            .any(|row| row.starts_with("memories.text ")),
        "pre-purge snapshot did not expose the canonical bait: {before_survivors:?}"
    );
    {
        let conn = Connection::open_with_flags(
            &snapshot_before,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        assert_eq!(
            fts_match_count(&conn, BAIT)?,
            1,
            "pre-purge FTS index did not expose its bait"
        );
        let tables = table_names(&conn)?;
        for shadow in [
            "fts_memories_config",
            "fts_memories_data",
            "fts_memories_docsize",
            "fts_memories_idx",
        ] {
            assert!(
                tables.iter().any(|name| name == shadow),
                "FTS shadow table {shadow} was not scanned"
            );
        }
    }

    // Purge through the same public operation used by CLI, JSON, and MCP.
    api.forget(&ForgetInput {
        id: public_id.clone(),
        action: Some("hard".into()),
        to_version: None,
        reason: Some("leak-path acceptance test".into()),
    })
    .await?;
    assert!(store.get_memory(public_id.clone()).await?.is_none());

    // Path 1: recall cannot return the bait or its public id.
    let recalled_after = recall_public_ids(&api).await?;
    assert!(
        !recalled_after.contains(&public_id),
        "recall leak: post-purge hits still contain {public_id}: {recalled_after:?}"
    );

    // Path 2: every live table, including discovered shadow tables, is clean.
    let live_survivors = store.read(|conn| survivor_columns(conn, BAIT)).await?;
    assert!(
        live_survivors.is_empty(),
        "database SQL leak after hard purge: {live_survivors:?}"
    );
    let live_fts_matches = store.read(|conn| fts_match_count(conn, BAIT)).await?;
    assert_eq!(live_fts_matches, 0, "FTS index leaked the purged bait");

    // A fresh snapshot is both query-clean and byte-clean. The byte scan is
    // essential: SQL alone cannot detect an old payload left in a free page.
    let snapshot_after = dir.path().join("after.db");
    let report = store.snapshot_to(&snapshot_after).await?;
    assert_eq!(report.integrity, "ok");
    let after_snapshot_survivors = snapshot_survivors(&snapshot_after, BAIT)?;
    assert!(
        after_snapshot_survivors.is_empty(),
        "backup SQL leak after hard purge: {after_snapshot_survivors:?}"
    );
    {
        let conn = Connection::open_with_flags(
            &snapshot_after,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        assert_eq!(
            fts_match_count(&conn, BAIT)?,
            0,
            "backup FTS index leaked the purged bait"
        );
    }
    assert!(
        !file_has_bait(&snapshot_after),
        "backup byte leak after hard purge: {}",
        snapshot_after.display()
    );

    // Path 3: a fresh tombstone-aware export contains neither payload nor id.
    let export_after = dir.path().join("after.jsonl");
    export(store, None, Some(&export_after)).await?;
    let after_export = std::fs::read_to_string(&export_after)
        .unwrap_or_else(|e| panic!("cannot read post-purge export: {e}"));
    assert!(
        !after_export.contains(BAIT),
        "export payload leak after hard purge"
    );
    assert!(
        !after_export.contains(&public_id),
        "export id leak after hard purge: {public_id}"
    );

    // Finally checkpoint all post-purge audit writes and scan the raw DB/WAL/SHM
    // bytes. A post-purge backup test that only queries SQL is insufficient.
    store.wal_checkpoint().await?;
    for raw in [
        db_path.clone(),
        sidecar(&db_path, "-wal"),
        sidecar(&db_path, "-shm"),
    ] {
        assert!(
            !file_has_bait(&raw),
            "raw database byte leak after hard purge: {}",
            raw.display()
        );
    }

    Ok(())
}
