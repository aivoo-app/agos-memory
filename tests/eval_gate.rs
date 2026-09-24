//! 0037/0052 acceptance: the offline eval gate.
//!
//! precision ≥ 0.90, recall ≥ 0.95, MRR ≥ 0.80, 0 leaks — measured
//! end-to-end on the shipped fixtures: each case's corpus is seeded through the
//! write path (embed + dedup + version + vector row + FTS trigger) and scored by
//! the real recall path. No network, no model access (deterministic hash
//! embedder), so the gate is reproducible anywhere.

use agos_memory::config::{Config, EmbedProvider};
use agos_memory::error::Result;
use agos_memory::eval::{CorpusItem, EvalBaseline, EvalCase, load_cases, run, validate_cases};
use std::path::Path;
use std::process::Command;

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

fn baseline() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval_baseline.json")
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
    let stored = EvalBaseline::load(&baseline())?;
    assert_eq!(stored.metrics.cases, cases.len() as u64);
    eval.metrics
        .check_baseline(&stored)
        .map_err(agos_memory::error::Error::InvalidInput)?;
    Ok(())
}

#[tokio::test]
async fn eval_is_deterministic() -> Result<()> {
    let cases = load_cases(&fixtures())?;
    let first = run(&eval_cfg(), &cases).await?;
    let second = run(&eval_cfg(), &cases).await?;
    assert_eq!(first.metrics, second.metrics, "hash eval must be stable");
    Ok(())
}

#[test]
fn baseline_drift_reports_baseline_and_measured() {
    let baseline = EvalBaseline::new(
        "v1.0.0",
        "abc123",
        agos_memory::eval::Metrics {
            precision: 0.95,
            recall: 0.98,
            mrr: 0.90,
            leaks: 0,
            cases: 20,
        },
    );
    let measured = agos_memory::eval::Metrics {
        precision: 0.93,
        ..baseline.metrics.clone()
    };
    let err = measured.check_baseline(&baseline).unwrap_err();
    assert!(err.contains("precision regression"), "{err}");
    assert!(err.contains("baseline=0.950000"), "{err}");
    assert!(err.contains("measured=0.930000"), "{err}");
    measured.check(0.90, 0.95, 0.80).unwrap();
}

#[tokio::test]
async fn perturbed_fixture_drift_fails_against_baseline() -> Result<()> {
    let mut cases = load_cases(&fixtures())?;
    // Keep the corpus and expected ids intact, but ask for a different fixture
    // item. This exercises the real recall runner, not a synthetic Metrics value.
    cases[0].query = "bicycle chain replaced at the city workshop".into();
    let measured = run(&eval_cfg(), &cases).await?.metrics;
    let baseline = EvalBaseline::load(&baseline())?;
    let err = measured
        .check_baseline(&baseline)
        .expect_err("a scoring-input perturbation must trip the committed baseline");
    assert!(err.contains("regression"), "{err}");
    assert!(err.contains("baseline="), "{err}");
    assert!(err.contains("measured="), "{err}");
    Ok(())
}

#[test]
fn baseline_update_is_explicit_and_roundtrips() -> Result<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("baseline.json");
    let baseline = EvalBaseline::new(
        "v1.0.0",
        "abc123",
        agos_memory::eval::Metrics {
            precision: 0.95,
            recall: 0.98,
            mrr: 0.90,
            leaks: 0,
            cases: 20,
        },
    );
    baseline.write(&path)?;
    assert_eq!(EvalBaseline::load(&path)?, baseline);
    Ok(())
}

#[test]
fn normal_cli_eval_does_not_write_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("eval.db");
    let config = dir.path().join("agos-memory.toml");
    let baseline_copy = dir.path().join("eval_baseline.json");
    std::fs::write(
        &config,
        format!(
            "db_path = '{}'\nagent_id = 'eval'\n\n[embed]\nprovider = 'hash'\n",
            db.display()
        ),
    )
    .unwrap();
    std::fs::copy(baseline(), &baseline_copy).unwrap();
    let before = std::fs::read(&baseline_copy).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agos-memory"))
        .env_remove("AGOS_EVAL_UPDATE_BASELINE")
        .args([
            "--config",
            config.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "eval",
            "--file",
            fixtures().to_str().unwrap(),
            "--baseline",
            baseline_copy.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run eval CLI");
    assert!(
        output.status.success(),
        "eval failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(before, std::fs::read(&baseline_copy).unwrap());
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
