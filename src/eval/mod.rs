//! Eval harness skeleton (issue 0014).
//!
//! Deterministic, offline: cases load from JSONL, retrieval is scored, and
//! precision/recall/MRR are reported against thresholds. Real retrieval
//! wiring lands in v0.3.0; the harness, metrics, and thresholds format are
//! fixed here so evals are comparable from the first commit.

use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

/// One eval case.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalCase {
    /// Stable case id.
    pub id: String,
    /// The query the agent would issue.
    pub query: String,
    /// Memory ids expected to be recalled (relevant set).
    pub relevant: Vec<String>,
    /// Memory ids that must NOT be recalled (e.g. forgotten, untrusted).
    pub forbidden: Vec<String>,
    /// Memory ids available to the candidate pool (candidate corpus ids).
    #[serde(default)]
    pub corpus: Vec<String>,
}

/// Loader for JSONL eval files.
pub fn load_cases(path: &Path) -> Result<Vec<EvalCase>> {
    let raw = std::fs::read_to_string(path).map_err(|e| {
        Error::InvalidInput(format!("cannot read eval file {}: {e}", path.display()))
    })?;
    let mut cases = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let case: EvalCase = serde_json::from_str(line)
            .map_err(|e| Error::InvalidInput(format!("eval line {}: {e}", i + 1)))?;
        cases.push(case);
    }
    if cases.is_empty() {
        return Err(Error::InvalidInput("eval file has no cases".into()));
    }
    Ok(cases)
}

/// Metrics for one run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Metrics {
    /// Relevant returned / relevant available.
    pub precision: f64,
    /// Relevant returned / relevant expected.
    pub recall: f64,
    /// Mean reciprocal rank of the first relevant hit.
    pub mrr: f64,
    /// Times a forbidden id was returned (must always be 0).
    pub leaks: u64,
    /// Cases run.
    pub cases: u64,
}

impl Metrics {
    /// Aggregate metrics over per-case results.
    ///
    /// `results` holds, per case: ranked returned ids, expected ids, forbidden ids.
    pub fn aggregate(results: &[CaseResult]) -> Metrics {
        let n = results.len() as f64;
        let mut precision = 0.0;
        let mut recall = 0.0;
        let mut mrr = 0.0;
        let mut leaks = 0;
        for r in results {
            let hits = r
                .returned
                .iter()
                .filter(|id| r.relevant.contains(id))
                .count() as f64;
            if !r.returned.is_empty() {
                precision += hits / r.returned.len() as f64;
            }
            if !r.relevant.is_empty() {
                recall += hits / r.relevant.len() as f64;
            }
            if let Some(rank) = r.returned.iter().position(|id| r.relevant.contains(id)) {
                mrr += 1.0 / (rank + 1) as f64;
            }
            leaks += r
                .returned
                .iter()
                .filter(|id| r.forbidden.contains(id))
                .count() as u64;
        }
        Metrics {
            precision: precision / n,
            recall: recall / n,
            mrr: mrr / n,
            leaks,
            cases: results.len() as u64,
        }
    }

    /// Check against thresholds; returns a failure description when unmet.
    pub fn check(
        &self,
        min_precision: f64,
        min_recall: f64,
        min_mrr: f64,
    ) -> std::result::Result<(), String> {
        if self.leaks != 0 {
            return Err(format!(
                "{} forbidden leak(s) — forbidden ids must never surface",
                self.leaks
            ));
        }
        if self.precision < min_precision {
            return Err(format!(
                "precision {:.3} < {min_precision:.3}",
                self.precision
            ));
        }
        if self.recall < min_recall {
            return Err(format!("recall {:.3} < {min_recall:.3}", self.recall));
        }
        if self.mrr < min_mrr {
            return Err(format!("mrr {:.3} < {min_mrr:.3}", self.mrr));
        }
        Ok(())
    }
}

/// Per-case result handed to [`Metrics::aggregate`].
#[derive(Debug, Clone)]
pub struct CaseResult {
    /// Ranked ids returned by the retriever.
    pub returned: Vec<String>,
    /// Ids that count as relevant.
    pub relevant: Vec<String>,
    /// Ids that must never appear.
    pub forbidden: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_perfect_run() {
        let r = vec![CaseResult {
            returned: vec!["a".into(), "b".into()],
            relevant: vec!["a".into()],
            forbidden: vec![],
        }];
        let m = Metrics::aggregate(&r);
        assert_eq!(m.precision, 0.5);
        assert_eq!(m.recall, 1.0);
        assert_eq!(m.mrr, 1.0);
        assert_eq!(m.leaks, 0);
        m.check(0.4, 0.9, 0.9).unwrap();
        m.check(0.6, 0.9, 0.9).unwrap_err();
    }

    #[test]
    fn metrics_leak_fails_hard() {
        let r = vec![CaseResult {
            returned: vec!["forgotten".into()],
            relevant: vec!["a".into()],
            forbidden: vec!["forgotten".into()],
        }];
        let m = Metrics::aggregate(&r);
        assert_eq!(m.leaks, 1);
        assert!(m.check(0.0, 0.0, 0.0).is_err());
    }

    #[test]
    fn load_cases_parses_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cases.jsonl");
        std::fs::write(
            &p,
            concat!(
                "{\"id\":\"c1\",\"query\":\"q\",\"relevant\":[\"m1\"],\"forbidden\":[\"m2\"]}\n",
                "\n",
                "{\"id\":\"c2\",\"query\":\"q2\",\"relevant\":[],\"forbidden\":[],\"corpus\":[\"m1\"]}\n"
            ),
        )
        .unwrap();
        let cases = load_cases(&p).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].relevant, vec!["m1".to_string()]);
    }
}
