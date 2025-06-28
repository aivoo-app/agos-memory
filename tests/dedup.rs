//! v0.2.0 gate: near-identical memories collapse into one row (D20).
//!
//! `remember()` must not let reformatted duplicates pile up. A second
//! near-identical write bumps `ref_count`, stamps `last_referenced_at`, links
//! `refines`, and tags the dedup cluster instead of inserting a new row.

mod common;

use agos_memory::memory::{persist, remember};
use agos_memory::observe::EXTRACTOR_VERSION;

/// Count rows / sum refcounts / count links for one cluster.
async fn stats(store: &agos_memory::storage::StoreHandle, needle: &str) -> (i64, i64, i64) {
    let needle = needle.to_string();
    store
        .read(move |conn| {
            let rows: i64 = conn.query_row(
                "SELECT count(*) FROM memories WHERE text LIKE ?1",
                [format!("%{needle}%")],
                |r| r.get(0),
            )?;
            let refs: i64 = conn.query_row(
                "SELECT COALESCE(SUM(ref_count), 0) FROM memories WHERE text LIKE ?1",
                [format!("%{needle}%")],
                |r| r.get(0),
            )?;
            let links: i64 = conn.query_row(
                "SELECT count(*) FROM memory_links l
                 JOIN memories m ON m.id = l.from_memory_id
                 WHERE m.text LIKE ?1 AND l.kind = 'refines'",
                [format!("%{needle}%")],
                |r| r.get(0),
            )?;
            Ok((rows, refs, links))
        })
        .await
        .unwrap()
}

/// Two texts that differ only in case/punctuation map to the identical vector,
/// so the second write must be treated as a duplicate of the first.
#[tokio::test]
async fn duplicate_write_bumps_refcount_and_links() {
    let (store, _dir) = common::store("dedup.db").await;
    let emb = common::NormEmbedder::new(common::DIM);

    let first = remember(
        &store,
        "semantic",
        "fact",
        "User prefers Rust for backend work",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();

    // Same words, different formatting → same normalized vector.
    let second = remember(
        &store,
        "semantic",
        "fact",
        "user prefers rust for backend work!",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();

    assert_eq!(
        second.public_id, first.public_id,
        "duplicate must resolve to the existing memory, not a new row"
    );

    let (rows, refs, links) = stats(&store, "backend work").await;
    assert_eq!(rows, 1, "duplicate must not create a second row");
    assert_eq!(refs, 1, "ref_count must be bumped exactly once");
    assert_eq!(links, 1, "one `refines` link must be recorded");

    let pid = first.public_id.clone();
    let (cluster, last_ref, updated) = store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT dedup_cluster_id, last_referenced_at, updated_at
                 FROM memories WHERE public_id = ?1",
                [&pid],
                |r| {
                    Ok((
                        r.get::<_, Option<i64>>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )?)
        })
        .await
        .unwrap();
    assert!(cluster.is_some(), "dedup_cluster_id must be tagged (D20)");
    assert!(last_ref.is_some(), "last_referenced_at must be stamped");
    assert!(updated >= first.created_at, "updated_at must move forward");
}

/// Genuinely different facts must NOT collapse — dedup is a similarity gate,
/// not a "same tier" gate.
#[tokio::test]
async fn distinct_facts_stay_separate() {
    let (store, _dir) = common::store("distinct.db").await;
    let emb = common::NormEmbedder::new(common::DIM);

    let a = remember(
        &store,
        "semantic",
        "fact",
        "User prefers Rust for backend work",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();
    let b = remember(
        &store,
        "semantic",
        "preference",
        "User drinks oat milk in the morning",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();

    assert_ne!(a.public_id, b.public_id, "unrelated facts must not dedup");
    let (rows, refs, links) = stats(&store, "%").await;
    assert_eq!(rows, 2);
    assert_eq!(refs, 0, "neither row was re-referenced");
    assert_eq!(links, 0, "no refines link between distinct facts");
}

/// A lower dedup threshold makes near-duplicates collapse; a threshold of 1.0
/// (never reached) disables dedup entirely. Proves the gate is configurable.
#[tokio::test]
async fn threshold_controls_dedup() {
    let (store, _dir) = common::store("threshold.db").await;
    let emb = common::NormEmbedder::new(common::DIM);

    let a = remember(
        &store,
        "semantic",
        "fact",
        "Deploys happen on Friday",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        1.0, // unreachable: dedup disabled
    )
    .await
    .unwrap();
    let b = remember(
        &store,
        "semantic",
        "fact",
        "deploys happen on friday",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        1.0,
    )
    .await
    .unwrap();

    assert_ne!(
        a.public_id, b.public_id,
        "dedup disabled at threshold 1.0 → both rows persist"
    );

    // Now with the real threshold, the same text collapses onto `b`.
    let c = remember(
        &store,
        "semantic",
        "fact",
        "Deploys happen on Friday",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();
    assert!(
        c.public_id == a.public_id || c.public_id == b.public_id,
        "with dedup on, the repeat attaches to one of the existing rows"
    );
}
