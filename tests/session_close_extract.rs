//! 0055 acceptance: `session close` / `idle-close` enqueue an `extract` job
//! so the worker runs the extractor on the just-closed session's turns.
//!
//! The enqueue is best-effort: the close must succeed even if the queue is
//! misbehaving, but when the queue is healthy a job row must exist.
//!
//! Note: `run_session` opens its own store handle (the store is single-writer
//! via flock), so the test opens a *separate* handle only after the CLI
//! commands have returned and released theirs.

use agos_memory::cli::remember::run_session;
use agos_memory::cli::root::SessionCmd;
use agos_memory::config::{Config, EmbedProvider};
use agos_memory::error::Result;
use agos_memory::storage::StoreHandle;

fn test_cfg(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        embed: agos_memory::config::EmbedConfig {
            provider: EmbedProvider::Hash,
            ..Default::default()
        },
        ..Config::default()
    }
}

/// Open a read-only handle on the DB file *after* the CLI commands have
/// finished and released theirs. The store is single-writer (flock), so two
/// handles on the same path cannot coexist.
async fn count_queued_extract_jobs(cfg: &Config) -> i64 {
    let store = StoreHandle::open(cfg, 1).await.unwrap();
    store
        .read(move |conn| {
            let c: i64 = conn
                .query_row(
                    "SELECT count(*) FROM jobs WHERE kind = 'extract' AND status = 'queued'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| agos_memory::error::Error::Storage(e.to_string()))?;
            Ok(c)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn session_close_enqueues_extract_job() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_cfg(&dir.path().join("s.db"));

    // Open a session, append a turn, then close it via the CLI path.
    run_session(&cfg, &SessionCmd::Open).await?;
    run_session(
        &cfg,
        &SessionCmd::Append {
            session: None,
            role: "user".into(),
            content: "I prefer tea with oat milk.".into(),
        },
    )
    .await?;
    run_session(&cfg, &SessionCmd::Close { session: None }).await?;

    // An extract job for the closed session must now be queued.
    let count = count_queued_extract_jobs(&cfg).await;
    assert!(
        count >= 1,
        "session close must enqueue at least one extract job, got {count}"
    );
    Ok(())
}

#[tokio::test]
async fn session_close_succeeds_even_if_enqueue_fails() -> Result<()> {
    // The enqueue is best-effort: the close must not fail even if the queue
    // rejects the job. We can't easily force the queue to fail here, so we
    // assert the close itself returns Ok and the store is consistent.
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_cfg(&dir.path().join("s2.db"));

    run_session(&cfg, &SessionCmd::Open).await?;
    run_session(
        &cfg,
        &SessionCmd::Append {
            session: None,
            role: "user".into(),
            content: "hello".into(),
        },
    )
    .await?;
    let r = run_session(&cfg, &SessionCmd::Close { session: None }).await;
    assert!(r.is_ok(), "session close must succeed: {r:?}");
    Ok(())
}

#[tokio::test]
async fn idle_close_enqueues_extract_for_every_closed_session() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = test_cfg(&dir.path().join("s3.db"));

    // Two open sessions, both idle (started at epoch 0).
    run_session(&cfg, &SessionCmd::Open).await?;
    run_session(&cfg, &SessionCmd::Open).await?;
    {
        let s = StoreHandle::open(&cfg, 1).await?;
        s.write(move |conn| {
            conn.execute(
                "UPDATE sessions SET started_at = 0 WHERE status = 'open'",
                [],
            )?;
            Ok(())
        })
        .await?;
    }

    run_session(&cfg, &SessionCmd::IdleClose).await?;

    let count = count_queued_extract_jobs(&cfg).await;
    assert!(
        count >= 2,
        "idle-close must enqueue at least one extract job per closed session, got {count}"
    );
    Ok(())
}
