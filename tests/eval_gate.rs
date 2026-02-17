//! 0037/0052 acceptance: the offline eval gate.
//!
//! precision ≥ 0.90, recall ≥ 0.95, MRR ≥ 0.80, 0 leaks — measured
//! end-to-end on the shipped fixtures: each case's corpus is seeded through the
//! write path (embed + dedup + version + vector row + FTS trigger) and scored by
//! the real recall path. No network, no model access (deterministic hash
//! embedder), so the gate is reproducible anywhere.

use agos_memory::config::{Config, EmbedProvider};
use agos_memory::error::Result;
use agos_memory::eval::{CorpusItem, EvalCase, load_cases, run, validate_cases};
use std::path::Path;

/// Hermetic config: deterministic in-process embedder; each case gets its own
/// temp database (assigned by the runner).
fn eval_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.embed.provider = EmbedProvider::Hash;
    cfg
}

/// The shipped fixture file.
fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval_cases.jsonl")
}

#[tokio::test]
async fn eval_gate_passes() -> Result<()> {
    let cases = load_cases(&fixtures())?;
    validate_cases(&cases)?;

    assert!(
        cases.len() >= 20,
        "the gate claims ≥20 cases, fixture file has {}",
        cases.len()
    );

    // A zero-leak claim is only meaningful if leak bait exists: untrusted or
    // deprecated memories that the default Strict policy must never inject.
    let with_bait = cases.iter().filter(|c| !c.forbidden.is_empty()).count();
    assert!(
        with_bait >= 10,
        "leak detection must be exercised: only {with_bait} of {} cases declare forbidden ids",
        cases.len()
    );

    let eval = run(&eval_cfg(), &cases).await?;

    for outcome in eval.imperfect() {
        eprintln!(
            "imperfect case {}: query={:?} missed={:?} leaked={:?} returned={:?}",
            outcome.id, outcome.query, outcome.missed, outcome.leaked, outcome.returned
        );
    }

    assert_eq!(
        eval.metrics.leaks, 0,
        "forbidden ids surfaced: untrusted/deprecated memories must never be injected"
    );
    assert_eq!(
        eval.metrics.cases,
        cases.len() as u64,
        "every case must be scored"
    );
    eval.metrics
        .check(0.90, 0.95, 0.80)
        .map_err(agos_memory::error::Error::InvalidInput)?;
    Ok(())
}

#[tokio::test]
async fn eval_rejects_a_case_that_cannot_be_searched() {
    // The exact failure mode of the first wiring (issue 0052): the corpus was a
    // list of ids with no text, so the harness scored an empty database and
    // reported a silent 0.000/0.000/0.000 result.
    let case = EvalCase {
        id: "id-only".into(),
        query: "anything".into(),
        relevant: vec!["m1".into()],
        forbidden: vec![],
        corpus: vec![],
        k: None,
    };
    let err = validate_cases(&[case]).unwrap_err();
    assert!(err.to_string().contains("corpus is empty"), "{err}");
}

#[tokio::test]
async fn eval_runner_fails_loudly_when_the_corpus_collapses() -> Result<()> {
    // Two corpus items with identical text dedup on write: the fixture id →
    // stored id mapping would become ambiguous. The runner must refuse to
    // score rather than silently attribute hits to the wrong memory.
    let case = EvalCase {
        id: "collapse".into(),
        query: "identical".into(),
        relevant: vec!["m1".into()],
        forbidden: vec![],
        corpus: vec![
            CorpusItem::new("m1", "identical corpus text"),
            CorpusItem::new("m2", "identical corpus text"),
        ],
        k: Some(1),
    };
    let err = run(&eval_cfg(), &[case]).await.unwrap_err();
    assert!(err.to_string().contains("deduped"), "{err}");
    Ok(())
}
