//! v0.2.0 gate: a fact written through `remember()` survives a restart.
//!
//! Losing memories on process restart is the failure this milestone exists to
//! prevent, so this suite closes the store entirely and reopens the same file.

mod common;

use agos_memory::memory::{persist, remember, sessions};
use agos_memory::observe::EXTRACTOR_VERSION;

/// `remember()` writes the row, its `memory_versions` v1, and its vector, and
/// all three are still there after the store is dropped and reopened.
#[tokio::test]
async fn fact_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("restart.db");
    let emb = common::NormEmbedder::new(common::DIM);

    let public_id = {
        let store = agos_memory::storage::StoreHandle::open(&common::cfg(&db), 2)
            .await
            .unwrap();
        let row = remember(
            &store,
            "semantic",
            "fact",
            "The user deploys AGOS on a single Hetzner box.",
            "user",
            0.9,
            &emb,
            EXTRACTOR_VERSION,
            agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
            persist::DEDUP_THRESHOLD,
        )
        .await
        .unwrap();
        assert_eq!(row.status, "active");
        assert_eq!(row.trust, "trusted");
        // Store is dropped here: writer thread joins, lock released, WAL flushed.
        row.public_id
    };

    let store = agos_memory::storage::StoreHandle::open(&common::cfg(&db), 2)
        .await
        .unwrap();

    let row = store
        .get_memory(public_id.clone())
        .await
        .unwrap()
        .expect("memory must survive the restart");
    assert_eq!(row.text, "The user deploys AGOS on a single Hetzner box.");
    assert_eq!(row.status, "active");
    assert_eq!(row.trust, "trusted");

    let pid = public_id.clone();
    let (versions, vec_rows, fts_hits) = store
        .read(move |conn| {
            let versions: i64 = conn.query_row(
                "SELECT count(*) FROM memory_versions mv
                 JOIN memories m ON m.id = mv.memory_id
                 WHERE m.public_id = ?1 AND mv.version = 1",
                [&pid],
                |r| r.get(0),
            )?;
            let vec_rows: i64 = conn.query_row(
                "SELECT count(*) FROM vec_memories v
                 JOIN memories m ON m.id = v.rowid
                 WHERE m.public_id = ?1",
                [&pid],
                |r| r.get(0),
            )?;
            let fts_hits: i64 = conn.query_row(
                "SELECT count(*) FROM fts_memories f
                 JOIN memories m ON m.id = f.rowid
                 WHERE m.public_id = ?1 AND fts_memories MATCH 'hetzner'",
                [&pid],
                |r| r.get(0),
            )?;
            Ok((versions, vec_rows, fts_hits))
        })
        .await
        .unwrap();

    assert_eq!(versions, 1, "version v1 row must persist");
    assert_eq!(vec_rows, 1, "vec_memories row must persist");
    assert_eq!(fts_hits, 1, "FTS5 index must still match after restart");
}

/// Sessions and their turns are equally durable, and the embedding pin
/// (`meta.embed_dim`) still guards the reopened database.
#[tokio::test]
async fn session_turns_and_embed_pin_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("session.db");

    let session_public = {
        let store = agos_memory::storage::StoreHandle::open(&common::cfg(&db), 2)
            .await
            .unwrap();
        let s = sessions::open_session(&store, "default").await.unwrap();
        sessions::append_turn(&store, s.id, "user", "remember: I ship on Fridays")
            .await
            .unwrap();
        sessions::append_turn(&store, s.id, "assistant", "noted")
            .await
            .unwrap();
        s.public_id
    };

    let store = agos_memory::storage::StoreHandle::open(&common::cfg(&db), 2)
        .await
        .unwrap();

    let id = sessions::session_id_by_public(&store, &session_public)
        .await
        .unwrap()
        .expect("session must survive the restart");
    let turns = sessions::session_turns(&store, id).await.unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].seq, 1);
    assert_eq!(turns[1].seq, 2);

    let open = sessions::get_open_session(&store, "default").await.unwrap();
    assert!(open.is_some(), "session stays open across restart");

    assert_eq!(store.embed_dim().await.unwrap(), common::DIM);
}

/// Degraded mode (`provider = none`) must also survive a restart — via FTS5
/// only, since there is no vector row to index.
#[tokio::test]
async fn degraded_memory_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("degraded.db");

    let public_id = {
        let store = agos_memory::storage::StoreHandle::open(&common::cfg(&db), 2)
            .await
            .unwrap();
        let row = agos_memory::memory::remember_degraded(
            &store,
            "episodic",
            "fact",
            "The staging box runs Debian 13.",
            "user",
            0.8,
            EXTRACTOR_VERSION,
            agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        )
        .await
        .unwrap();
        row.public_id
    };

    let store = agos_memory::storage::StoreHandle::open(&common::cfg(&db), 2)
        .await
        .unwrap();

    let pid = public_id.clone();
    let (status, embed_status, vec_rows, fts_hits) = store
        .read(move |conn| {
            let row: (String, String) = conn.query_row(
                "SELECT status, embed_status FROM memories WHERE public_id = ?1",
                [&pid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let vec_rows: i64 = conn.query_row(
                "SELECT count(*) FROM vec_memories v
                 JOIN memories m ON m.id = v.rowid WHERE m.public_id = ?1",
                [&pid],
                |r| r.get(0),
            )?;
            let fts_hits: i64 = conn.query_row(
                "SELECT count(*) FROM fts_memories f
                 JOIN memories m ON m.id = f.rowid
                 WHERE m.public_id = ?1 AND fts_memories MATCH 'debian'",
                [&pid],
                |r| r.get(0),
            )?;
            Ok((row.0, row.1, vec_rows, fts_hits))
        })
        .await
        .unwrap();

    assert_eq!(status, "active");
    assert_eq!(embed_status, "skipped");
    assert_eq!(vec_rows, 0, "degraded mode writes no vector row");
    assert_eq!(fts_hits, 1, "degraded memory is still keyword-recallable");
}
