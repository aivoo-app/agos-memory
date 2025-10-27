//! Hybrid recall acceptance tests (issue 0031).
//!
//! Gate: trusted/untrusted/pending/episodic exclusion, RRF fusion ranking,
//! degraded (BM25-only) mode when the embedder refuses, and the empty-query
//! guard. The `NormEmbedder` maps distinct wordings to orthogonal vectors, so
//! an exact-text query is a similarity-1.0 vec hit while paraphrases stay
//! similarity-0 — the KNN scan still returns every row (k exceeds row count),
//! which is exactly what makes the in-scan trust filter observable.

mod common;

use common::{DIM, NormEmbedder, store};

use agos_memory::config::{Config, TrustPolicy};
use agos_memory::embed::Embedder;
use agos_memory::error::Result;
use agos_memory::memory::extract::Candidate;
use agos_memory::memory::{DEDUP_THRESHOLD, persist_candidate_full};
use agos_memory::recall::{RecallQuery, recall};

const EXTRACTOR: &str = "test-v1";
const PENDING_THRESHOLD: f64 = 0.4;

/// Persist one candidate and return its public id.
async fn put(
    store: &agos_memory::storage::StoreHandle,
    embedder: &NormEmbedder,
    text: &str,
    source_kind: &str,
    confidence: f64,
) -> String {
    let cand = Candidate {
        tier: "semantic".to_string(),
        kind: "fact".to_string(),
        text: text.to_string(),
        importance: 0.5,
        confidence,
        session_independent: true,
        source_seq: 0,
    };
    persist_candidate_full(
        store,
        &cand,
        embedder,
        EXTRACTOR,
        source_kind,
        PENDING_THRESHOLD,
        DEDUP_THRESHOLD,
    )
    .await
    .unwrap()
    .row
    .public_id
}

fn query(text: &str) -> RecallQuery {
    RecallQuery::new(text, &Config::default().recall)
}

/// Embedder that always refuses (provider outage simulation).
struct FailingEmbedder;

#[async_trait::async_trait]
impl Embedder for FailingEmbedder {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(agos_memory::error::Error::Embedder("provider down".into()))
    }

    fn model(&self) -> &str {
        "failing-mock"
    }

    fn dim(&self) -> usize {
        DIM
    }
}

#[tokio::test]
async fn strict_excludes_untrusted_and_fenced_opts_in() {
    let (store, _dir) = store("recall_trust").await;
    let emb = NormEmbedder::new(DIM);

    let trusted_id = put(
        &store,
        &emb,
        "deploy the service with blue green rollout",
        "user",
        0.9,
    )
    .await;
    let untrusted_id = put(
        &store,
        &emb,
        "deploy the service with canary rollout",
        "web",
        0.9,
    )
    .await;

    // Strict (default): the untrusted row is filtered INSIDE the vec scan and
    // by the FTS join — it appears nowhere.
    let strict = recall(&store, &emb, &query("deploy service rollout"))
        .await
        .unwrap();
    assert_eq!(
        strict.hits.len(),
        1,
        "strict must return only the trusted row"
    );
    assert_eq!(strict.hits[0].public_id, trusted_id);
    assert_eq!(strict.hits[0].trust, "trusted");
    assert!(!strict.degraded);

    // Fenced: the untrusted row is retrieved and must be marked so the caller
    // can render it inside a data fence (D29).
    let mut fenced = query("deploy service rollout");
    fenced.trust_policy = TrustPolicy::Fenced;
    let fenced = recall(&store, &emb, &fenced).await.unwrap();
    let ids: Vec<&str> = fenced.hits.iter().map(|h| h.public_id.as_str()).collect();
    assert!(ids.contains(&trusted_id.as_str()));
    assert!(ids.contains(&untrusted_id.as_str()));
    let untrusted_hit = fenced
        .hits
        .iter()
        .find(|h| h.public_id == untrusted_id)
        .unwrap();
    assert_eq!(untrusted_hit.trust, "untrusted");
}

#[tokio::test]
async fn rrf_fusion_prefers_double_leg_hits() {
    let (store, _dir) = store("recall_fuse").await;
    let emb = NormEmbedder::new(DIM);

    let a_id = put(
        &store,
        &emb,
        "kafka consumer lag alerting runbook",
        "user",
        0.9,
    )
    .await;
    let _b_id = put(&store, &emb, "kafka consumer lag dashboard", "user", 0.9).await;
    let _c_id = put(&store, &emb, "postgres backup rotation policy", "user", 0.9).await;

    // Exact text of A → similarity 1.0 on the vec leg (rank 1) and an FTS hit
    // (rank 1) → A must fuse to the top with both components present.
    let report = recall(&store, &emb, &query("kafka consumer lag alerting runbook"))
        .await
        .unwrap();

    assert!(!report.hits.is_empty());
    assert_eq!(report.hits[0].public_id, a_id);
    let comps = &report.hits[0].components;
    assert!(
        comps.sim.unwrap() > 0.99,
        "exact-text query must be a vec hit"
    );
    assert_eq!(
        comps.bm25_rank,
        Some(0),
        "exact-text query must lead FTS too"
    );
    assert!(comps.rrf > 0.0);
    // All rows fit inside k, so every stored row surfaces via at least the
    // vec leg; the exact-match row must still lead.
    assert!(report.hits.len() >= 2);
}

#[tokio::test]
async fn degraded_bm25_only_when_embedder_fails() {
    let (store, _dir) = store("recall_degraded").await;
    let emb = NormEmbedder::new(DIM);
    put(
        &store,
        &emb,
        "kafka partition rebalance runbook",
        "user",
        0.9,
    )
    .await;
    put(&store, &emb, "postgres vacuum schedule", "user", 0.9).await;

    let report = recall(&store, &FailingEmbedder, &query("kafka rebalance runbook"))
        .await
        .unwrap();

    assert!(report.degraded, "embedder refusal must degrade, not fail");
    assert_eq!(
        report.hits.len(),
        1,
        "BM25-only: only the keyword-matching row"
    );
    assert!(report.hits[0].components.sim.is_none());
    assert!(report.hits[0].components.bm25_rank.is_some());
}

#[tokio::test]
async fn pending_excluded_unless_opted_in() {
    let (store, _dir) = store("recall_pending").await;
    let emb = NormEmbedder::new(DIM);

    let active_id = put(&store, &emb, "final migration rollout plan", "user", 0.9).await;
    let pending_id = put(&store, &emb, "draft migration rollout plan", "user", 0.2).await;

    // Default: pending rows are invisible on both legs.
    let default_q = query("migration rollout plan");
    assert!(!default_q.include_pending);
    let report = recall(&store, &emb, &default_q).await.unwrap();
    let ids: Vec<&str> = report.hits.iter().map(|h| h.public_id.as_str()).collect();
    assert!(!ids.contains(&pending_id.as_str()));
    assert!(ids.contains(&active_id.as_str()));

    // Opt-in: pending rows surface (still filtered by trust).
    let mut opt_in = query("migration rollout plan");
    opt_in.include_pending = true;
    let report = recall(&store, &emb, &opt_in).await.unwrap();
    let ids: Vec<&str> = report.hits.iter().map(|h| h.public_id.as_str()).collect();
    assert!(ids.contains(&pending_id.as_str()));
    assert!(ids.contains(&active_id.as_str()));
}

#[tokio::test]
async fn episodic_excluded_unless_opted_in() {
    let (store, _dir) = store("recall_episodic").await;
    let emb = NormEmbedder::new(DIM);

    let cand = Candidate {
        tier: "episodic".to_string(),
        kind: "note".to_string(),
        text: "standup meeting notes about the release".to_string(),
        importance: 0.5,
        confidence: 0.9,
        session_independent: false,
        source_seq: 0,
    };
    let episodic_id = persist_candidate_full(
        &store,
        &cand,
        &emb,
        EXTRACTOR,
        "user",
        PENDING_THRESHOLD,
        DEDUP_THRESHOLD,
    )
    .await
    .unwrap()
    .row
    .public_id;

    let report = recall(&store, &emb, &query("standup meeting notes release"))
        .await
        .unwrap();
    assert!(
        report.hits.iter().all(|h| h.public_id != episodic_id),
        "episodic must be excluded by default (D26)"
    );

    let mut opt_in = query("standup meeting notes release");
    opt_in.include_episodic = true;
    let report = recall(&store, &emb, &opt_in).await.unwrap();
    assert!(
        report.hits.iter().any(|h| h.public_id == episodic_id),
        "episodic opt-in must surface the row"
    );
}

#[tokio::test]
async fn empty_query_is_rejected() {
    let (store, _dir) = store("recall_empty").await;
    let emb = NormEmbedder::new(DIM);
    let err = recall(&store, &emb, &query("   ")).await.unwrap_err();
    assert!(err.to_string().contains("empty"));
}
