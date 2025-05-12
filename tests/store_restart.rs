//! Integration: the store survives a full close/reopen cycle, and the
//! one-process-per-database lock behaves (v0.1.0 acceptance, "facts survive
//! restart" gate — v0.1.0 slice).

use agos_memory::config::Config;
use agos_memory::storage::{NewMemory, StoreHandle};

fn test_config(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

#[tokio::test]
async fn memories_survive_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restart.db");

    let inserted = {
        let store = StoreHandle::open(&test_config(&path), 2).await.unwrap();
        let row = store
            .insert_memory(NewMemory {
                tier: "semantic".into(),
                kind: "fact".into(),
                text: "the deploy window is 02:00 UTC".into(),
                source_kind: "user".into(),
            })
            .await
            .unwrap();
        (row.public_id.clone(), row.text.clone(), row.tier.clone())
        // `store` (and its writer thread + lock) drops here: a real close.
    };

    let (public_id, text, tier) = inserted;
    let reopened = StoreHandle::open(&test_config(&path), 2).await.unwrap();
    let got = reopened
        .get_memory(public_id)
        .await
        .unwrap()
        .expect("memory must survive the restart");
    assert_eq!(got.text, text);
    assert_eq!(got.tier, tier);
    assert_eq!(got.status, "active");
}

#[tokio::test]
async fn all_inserted_rows_are_there_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bulk.db");

    {
        let store = StoreHandle::open(&test_config(&path), 4).await.unwrap();
        for i in 0..10 {
            store
                .insert_memory(NewMemory {
                    tier: "episodic".into(),
                    kind: "note".into(),
                    text: format!("event number {i}"),
                    source_kind: "agent".into(),
                })
                .await
                .unwrap();
        }
        assert_eq!(
            store.memory_counts().await.unwrap(),
            vec![("active".to_string(), 10)]
        );
    }

    let reopened = StoreHandle::open(&test_config(&path), 4).await.unwrap();
    assert_eq!(
        reopened.memory_counts().await.unwrap(),
        vec![("active".to_string(), 10)]
    );
}

#[tokio::test]
async fn lock_is_refused_while_open_and_released_after_drop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock.db");

    let first = StoreHandle::open(&test_config(&path), 1).await.unwrap();

    // While the first handle is alive, a second open is refused with the
    // holding pid in the message.
    let err = StoreHandle::open(&test_config(&path), 1).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("one process per database"), "got: {msg}");
    assert!(msg.contains(&format!("pid {}", std::process::id())),);

    // After the handle drops, the lock is released and a reopen succeeds.
    drop(first);
    let _second = StoreHandle::open(&test_config(&path), 1).await.unwrap();
}
