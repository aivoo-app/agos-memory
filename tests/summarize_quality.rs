//! Summarization quality gate (issue 0049).
//!
//! Runs every case in `fixtures/summarize_cases.jsonl` through the real
//! `summarize_by_id` pipeline with a canned `MockChat` response, then scores
//! the generated summary against the human-written `reference_summary` with
//! ROUGE-L. The milestone acceptance criterion is **mean ROUGE-L ≥ 0.85**.
//!
//! Hermetic by design: `MockChat` stands in for the provider, so CI measures
//! the summarization + scoring pipeline deterministically, never a live model.

use agos_memory::config::Config;
use agos_memory::eval::rouge_l;
use agos_memory::llm::{ChatClient, MockChat};
use agos_memory::storage::StoreHandle;
use serde::Deserialize;
use std::sync::Arc;

/// The v0.4.0 milestone acceptance threshold.
const ROUGE_L_GATE: f64 = 0.85;

#[derive(Debug, Deserialize)]
struct Case {
    id: String,
    tier: String,
    text: String,
    llm_response: String,
    reference_summary: String,
}

fn load_cases() -> Vec<Case> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/summarize_cases.jsonl");
    let raw = std::fs::read_to_string(&path).expect("read fixtures/summarize_cases.jsonl");
    raw.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| serde_json::from_str(l).expect("fixture line must be valid JSON"))
        .collect()
}

#[tokio::test]
async fn summarize_quality_meets_rouge_l_gate() {
    let cases = load_cases();
    assert!(
        cases.len() >= 100,
        "0049 requires 100 fixtures (got {})",
        cases.len()
    );

    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: dir.path().join("quality.db"),
        ..Config::default()
    };
    let store = StoreHandle::open(&cfg, 2).await.unwrap();

    // Seed every case through the store, then summarize with the canned LLM.
    let mut ids = Vec::with_capacity(cases.len());
    for case in &cases {
        let row = store
            .insert_memory(agos_memory::storage::NewMemory {
                text: case.text.clone(),
                tier: case.tier.clone(),
                kind: "fact".into(),
                source_kind: "user".into(),
            })
            .await
            .unwrap();
        ids.push(row.id);
    }

    let mut scores = Vec::with_capacity(cases.len());
    let mut case_outcomes = Vec::with_capacity(cases.len());
    for (case, id) in cases.iter().zip(&ids) {
        // Each case carries its own canned response — a fresh MockChat per call.
        let case_chat: Arc<dyn ChatClient> = Arc::new(MockChat::fixed(case.llm_response.clone()));
        let report = agos_memory::memory::summarize_by_id(&store, &case_chat, &cfg, *id, false)
            .await
            .unwrap();
        assert_eq!(
            report.summary_text, case.llm_response,
            "pipeline must store the LLM response verbatim"
        );
        let score = rouge_l(&case.reference_summary, &report.summary_text);
        scores.push(score);
        case_outcomes.push((case.id.clone(), score, report.quality_score));
    }

    let mean: f64 = scores.iter().sum::<f64>() / scores.len() as f64;
    for (id, score, quality) in &case_outcomes {
        println!("{id}: rouge_l_vs_reference={score:.3} quality_score_vs_source={quality:.3}");
    }
    println!(
        "=== mean ROUGE-L over {} fixtures: {:.4} (gate ≥ {}) ===",
        scores.len(),
        mean,
        ROUGE_L_GATE
    );

    // Every case must produce a real, non-degenerate summary.
    assert!(
        scores.iter().all(|s| *s > 0.0),
        "no case may score 0 — that means the pipeline dropped the summary"
    );
    assert!(
        mean >= ROUGE_L_GATE,
        "mean ROUGE-L {mean:.4} below the {ROUGE_L_GATE} acceptance gate (0049)"
    );
}
