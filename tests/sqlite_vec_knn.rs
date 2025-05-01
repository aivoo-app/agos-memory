//! Integration: sqlite-vec KNN with metadata hard filters over the real store
//! open path (v0.1.0 acceptance: "vec0 KNN verified").

use agos_memory::config::Config;
use agos_memory::embed::{Embedder, HashEmbedder};
use agos_memory::storage::StoreHandle;

/// The store's vec table is created with the pinned dim (1536 default).
const DIM: usize = 1536;

/// Encode status the way the vec0 metadata column expects it
/// (0=active, 1=pending, 2=deprecated, 3=deleted — matches the CHECK order).
const STATUS_ACTIVE: i64 = 0;
const STATUS_DEPRECATED: i64 = 2;

fn blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[tokio::test]
async fn knn_respects_tier_and_status_hard_filters() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join("knn.db"),
        ..Config::default()
    };
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let embedder = HashEmbedder::new(DIM);
    let texts = [
        "user prefers dark mode interfaces",  // rowid 1: semantic/active
        "user prefers dark mode interfaces!", // rowid 2: semantic/deprecated — closest but must be excluded
        "quarterly revenue forecast meeting", // rowid 3: episodic/active — off-topic
    ];
    let vectors = embedder.embed(&texts.map(String::from)).await.unwrap();

    // Insert straight into vec_memories through the single writer, mirroring
    // what the v0.2.0 write path will do.
    let vecs = vectors.clone();
    let tier_1 = "semantic".to_string();
    let tier_2 = "semantic".to_string();
    let tier_3 = "episodic".to_string();
    store
        .write(move |conn| {
            for (rowid, (v, tier, status)) in [
                (1, (&vecs[0], &tier_1, STATUS_ACTIVE)),
                (2, (&vecs[1], &tier_2, STATUS_DEPRECATED)),
                (3, (&vecs[2], &tier_3, STATUS_ACTIVE)),
            ] {
                conn.execute(
                    "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                     VALUES (?1, ?2, ?3, ?4, 0, 'fact', 0)",
                    rusqlite::params![rowid, blob(v), tier, status],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    // Query with rowid 1's vector; deprecated rowid 2 (same-ish text) and
    // episodic rowid 3 must be filtered *inside* the KNN scan.
    let query = blob(&vectors[0]);
    let rows: Vec<i64> = store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT rowid FROM vec_memories
                 WHERE embedding MATCH ?1 AND k = 5
                   AND status = 0 AND tier = 'semantic'
                 ORDER BY distance",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![query], |r| r.get::<_, i64>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap();

    assert_eq!(rows, vec![1], "only the semantic/active row may surface");
}

#[tokio::test]
async fn distance_to_an_identical_vector_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join("zero.db"),
        ..Config::default()
    };
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    let embedder = HashEmbedder::new(DIM);
    let v = embedder
        .embed(&["exact match only".to_string()])
        .await
        .unwrap();
    let v0 = v[0].clone();

    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO vec_memories(rowid, embedding, tier, status, trust, kind, pinned)
                 VALUES (7, ?1, 'semantic', 0, 0, 'fact', 0)",
                rusqlite::params![blob(&v0)],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let query = blob(&v[0]);
    let (rowid, distance): (i64, f64) = store
        .read(move |conn| {
            conn.query_row(
                "SELECT rowid, distance FROM vec_memories
                 WHERE embedding MATCH ?1 AND k = 1",
                rusqlite::params![query],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(Into::into)
        })
        .await
        .unwrap();

    assert_eq!(rowid, 7);
    assert!(
        distance.abs() < 1e-5,
        "identical vector, distance ~0, got {distance}"
    );
}
