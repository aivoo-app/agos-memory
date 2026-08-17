//! OpenClaw Markdown ingest acceptance.
//!
//! Exercises the CLI module against a real StoreHandle and HashEmbedder. The
//! tests assert source references and trust from SQLite, not only returned
//! values, and prove dry-run, idempotency, edits, stale reporting, and
//! redaction.

use agos_memory::cli::ingest::ingest;
use agos_memory::config::{Config, EmbedProvider};
use agos_memory::storage::StoreHandle;

async fn config(root: &std::path::Path) -> Config {
    let mut cfg = Config {
        db_path: root.join("memories.db"),
        ..Config::default()
    };
    cfg.embed.provider = EmbedProvider::Hash;
    cfg
}

async fn open(cfg: &Config) -> StoreHandle {
    StoreHandle::open(cfg, 2).await.expect("open store")
}

async fn rows(store: &StoreHandle) -> Vec<(String, String, String, String, String)> {
    store
        .read(|conn| {
            let mut stmt = conn.prepare(
                "SELECT public_id, text, source_kind, trust, source_ref
                 FROM memories ORDER BY public_id",
            )?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .expect("read memories")
}

fn fixture(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("memory")).unwrap();
    std::fs::write(
        dir.join("MEMORY.md"),
        "# OpenClaw\n\n- Deploys happen on Thursdays.\n\nThe staging cluster uses Rust.\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("memory/2026-09-24.md"),
        "# Daily\n\n- Reviewed the production dashboard.\n",
    )
    .unwrap();
}

#[tokio::test]
async fn ingests_markdown_with_real_line_provenance_and_untrusted_trust() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("openclaw");
    fixture(&root);
    let cfg = config(dir.path()).await;
    let report = ingest(&cfg, &root, false).await.unwrap();
    assert_eq!(report.seen, 3);
    assert_eq!(report.new, 3);
    assert_eq!(report.updated, 0);
    assert_eq!(report.stale, 0);

    let store = open(&cfg).await;
    let rows = rows(&store).await;
    assert_eq!(rows.len(), 3);
    let expected_sources = [
        ("MEMORY.md:3", "Deploys happen on Thursdays."),
        ("MEMORY.md:5", "The staging cluster uses Rust."),
        (
            "memory/2026-09-24.md:3",
            "Reviewed the production dashboard.",
        ),
    ];
    for (source_ref, source_text) in expected_sources {
        let row = rows
            .iter()
            .find(|row| row.4 == source_ref)
            .unwrap_or_else(|| panic!("missing source reference {source_ref}; rows={rows:?}"));
        assert_eq!(row.1, source_text);
        assert_eq!(row.2, "file");
        assert_eq!(row.3, "untrusted");
        let line = source_ref
            .rsplit_once(':')
            .map(|(_, line)| line.parse::<usize>().unwrap())
            .unwrap();
        let file = root.join(source_ref.rsplit_once(':').unwrap().0);
        let file_contents = std::fs::read_to_string(file).unwrap();
        let source_line = file_contents.lines().nth(line - 1).unwrap().trim();
        assert!(
            source_line.contains(source_text),
            "source line does not match: {source_line}"
        );
    }
    let (sessions, turns): (i64, i64) = store
        .read(|conn| {
            Ok((
                conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?,
                conn.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((sessions, turns), (1, 1));
}

#[tokio::test]
async fn unchanged_tree_is_a_noop_and_edited_line_creates_a_version() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("openclaw");
    fixture(&root);
    let cfg = config(dir.path()).await;
    assert_eq!(ingest(&cfg, &root, false).await.unwrap().new, 3);
    let second = ingest(&cfg, &root, false).await.unwrap();
    assert_eq!(second.new, 0);
    assert_eq!(second.updated, 0);
    assert_eq!(second.unchanged, 3);

    let store = open(&cfg).await;
    let before: i64 = store
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM memory_versions", [], |r| r.get(0))?))
        .await
        .unwrap();
    drop(store);

    std::fs::write(
        root.join("MEMORY.md"),
        "# OpenClaw\n\n- Deploys happen on Fridays.\n\nThe staging cluster uses Rust.\n",
    )
    .unwrap();
    let edited = ingest(&cfg, &root, false).await.unwrap();
    assert_eq!(edited.new, 0);
    assert_eq!(edited.updated, 1);
    assert_eq!(edited.unchanged, 2);

    let store = open(&cfg).await;
    let after: i64 = store
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM memory_versions", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(after, before + 1);
}

#[tokio::test]
async fn deleted_source_is_reported_but_not_silently_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("openclaw");
    fixture(&root);
    let cfg = config(dir.path()).await;
    ingest(&cfg, &root, false).await.unwrap();
    std::fs::remove_file(root.join("memory/2026-09-24.md")).unwrap();

    let report = ingest(&cfg, &root, false).await.unwrap();
    assert_eq!(report.seen, 2);
    assert_eq!(report.stale, 1);
    let store = open(&cfg).await;
    assert_eq!(
        rows(&store).await.len(),
        3,
        "stale source remains reviewable"
    );
}

#[tokio::test]
async fn dry_run_does_not_write_memories_or_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("openclaw");
    fixture(&root);
    let cfg = config(dir.path()).await;
    let report = ingest(&cfg, &root, true).await.unwrap();
    assert!(report.dry_run);
    assert_eq!(report.new, 3);

    let store = open(&cfg).await;
    assert!(rows(&store).await.is_empty());
    let manifest_count: i64 = store
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM meta WHERE key = 'openclaw_ingest_manifest_v1'",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(manifest_count, 0);
}

#[tokio::test]
async fn ingest_redacts_secrets_before_storage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("openclaw");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("MEMORY.md"),
        "# Notes\n\n- The deploy key is sk-liveSECRET1234567890.\n",
    )
    .unwrap();
    let cfg = config(dir.path()).await;
    ingest(&cfg, &root, false).await.unwrap();
    let store = open(&cfg).await;
    let text = rows(&store).await[0].1.clone();
    assert!(!text.contains("sk-liveSECRET1234567890"));
    assert!(text.contains("[REDACTED]"));
}
