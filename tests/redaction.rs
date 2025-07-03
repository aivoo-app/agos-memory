//! v0.2.0 gate: secrets are redacted before storage *and* before embedding (D21).
//!
//! Two independent leaks matter — a secret sitting in `memories.text`, and a
//! secret riding along in the provider payload — so this suite asserts both:
//! the row text is clean, and every string handed to the embedder is clean.

mod common;

use std::sync::{Arc, Mutex};

use agos_memory::memory::{REDACTED, persist, redact, remember};
use agos_memory::observe::EXTRACTOR_VERSION;

const SECRET: &str = "sk-liveSECRET1234567890";

async fn remembered_text(store: &agos_memory::storage::StoreHandle, public_id: &str) -> String {
    let pid = public_id.to_string();
    store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT text FROM memories WHERE public_id = ?1",
                [&pid],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap()
}

/// The pattern set itself: each documented secret shape is replaced.
#[test]
fn patterns_cover_the_documented_shapes() {
    let cases = [
        ("my key is sk-abcdefgh12345 ok", "sk-abcdefgh12345"),
        ("auth: Bearer token_value_123", "token_value_123"),
        ("password=hunter2!", "hunter2!"),
        ("SECRET: s3cr3t-value", "s3cr3t-value"),
        ("api-key: abc123def", "abc123def"),
    ];
    for (input, secret) in cases {
        let out = redact(input);
        assert!(
            !out.contains(secret),
            "pattern leak: {secret:?} survived in {out:?}"
        );
        assert!(out.contains(REDACTED), "expected a marker in {out:?}");
    }

    let pem = "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEE\n-----END EC PRIVATE KEY-----";
    assert_eq!(redact(pem), REDACTED, "PEM blocks are removed whole");

    // Benign text is untouched: redaction must not mangle normal memories.
    let benign = "The user prefers tabs over spaces and deploys on Fridays.";
    assert_eq!(redact(benign), benign);
}

/// A secret typed as a user fact never lands in `memories.text`.
#[tokio::test]
async fn secret_never_reaches_the_database() {
    let (store, _dir) = common::store("redact-db.db").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let emb = common::RecordingEmbedder::new(common::DIM, seen.clone());

    let row = remember(
        &store,
        "semantic",
        "fact",
        &format!("The deploy token is {SECRET} and must stay out of the log."),
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();

    let stored = remembered_text(&store, &row.public_id).await;
    assert!(
        !stored.contains(SECRET),
        "secret leaked into memories.text: {stored:?}"
    );
    assert!(stored.contains(REDACTED), "expected marker, got {stored:?}");
    // The context around the secret must survive: only the secret is dropped.
    assert!(stored.contains("deploy token"), "over-redacted: {stored:?}");

    // The version row is a copy of the text, so it must be clean too.
    let pid = row.public_id.clone();
    let version_text: String = store
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT text FROM memory_versions WHERE memory_id =
                    (SELECT id FROM memories WHERE public_id = ?1) ORDER BY version LIMIT 1",
                [&pid],
                |r| r.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        !version_text.contains(SECRET),
        "secret leaked into memory_versions.text: {version_text:?}"
    );
}

/// Redaction happens *before* embedding, so the provider never sees the secret.
#[tokio::test]
async fn secret_never_reaches_the_embed_payload() {
    let (store, _dir) = common::store("redact-payload.db").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let emb = common::RecordingEmbedder::new(common::DIM, seen.clone());

    remember(
        &store,
        "semantic",
        "fact",
        "Database password=SuperSecret42 for the staging box.",
        "user",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();

    let payloads = seen.lock().unwrap().clone();
    assert!(!payloads.is_empty(), "embedder must have been called");
    for payload in payloads {
        assert!(
            !payload.contains("SuperSecret42"),
            "secret reached the embed payload: {payload:?}"
        );
        assert!(
            !payload.contains(SECRET),
            "unrelated secret shape in payload: {payload:?}"
        );
    }
}

/// Untrusted provenance does not exempt a memory from redaction.
#[tokio::test]
async fn untrusted_sources_are_redacted_too() {
    let (store, _dir) = common::store("redact-untrusted.db").await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let emb = common::RecordingEmbedder::new(common::DIM, seen.clone());

    let row = remember(
        &store,
        "episodic",
        "fact",
        &format!("Scraped page contained api_key={SECRET} inside a snippet."),
        "tool",
        0.9,
        &emb,
        EXTRACTOR_VERSION,
        agos_memory::memory::PENDING_THRESHOLD_DEFAULT,
        persist::DEDUP_THRESHOLD,
    )
    .await
    .unwrap();

    assert_eq!(row.trust, "untrusted");
    let stored = remembered_text(&store, &row.public_id).await;
    assert!(
        !stored.contains(SECRET),
        "untrusted path leaked a secret: {stored:?}"
    );
    for payload in seen.lock().unwrap().iter() {
        assert!(
            !payload.contains(SECRET),
            "untrusted secret reached the payload: {payload:?}"
        );
    }
}
