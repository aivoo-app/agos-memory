//! Offline eval harness (issue 0014; wired end-to-end in issue 0052).
//!
//! Deterministic and offline: cases load from JSONL, the candidate corpus is
//! seeded through the **write path** (redact → embed → dedup → insert + FTS
//! trigger + vector row), the real [`crate::recall`] path answers each query,
//! and precision/recall/MRR plus a hard leak count are reported against
//! thresholds.
//!
//! Fixtures must carry text: an id-only corpus cannot be searched, and an
//! empty corpus is a hard error ([`validate_cases`]) rather than a silent zero
//! (that silent zero is what made the first wiring useless — issue 0052).
//!
//! Per case, `k` defaults to `[recall] top_k`; fixtures set `k = |relevant|`
//! so `precision` is precision@R — any distractor that outranks a relevant
//! memory displaces it and lowers the score.

use std::path::Path;

use serde::Deserialize;

use crate::error::{Error, Result};

pub mod rouge;
pub mod runner;

pub use rouge::{lcs_len, rouge_l, tokenize};
pub use runner::{CaseOutcome, EvalRun, run, run_cases};

/// One candidate memory available to a case.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CorpusItem {
    /// Fixture-local id, referenced by `relevant` / `forbidden`.
    pub id: String,
    /// The memory text to store (must be non-empty).
    pub text: String,
    /// Provenance; `web`/`tool`/`import` seeds `trust='untrusted'`, which the
    /// default Strict recall policy must never inject — leak bait.
    #[serde(default = "default_source_kind")]
    pub source_kind: String,
    /// Seed the item as soft-deprecated: it must never be recalled.
    #[serde(default)]
    pub deprecated: bool,
}

impl CorpusItem {
    /// Convenience constructor for tests/builders.
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            source_kind: default_source_kind(),
            deprecated: false,
        }
    }

    /// Mark the item as untrusted (`source_kind = "tool"`): leak bait.
    pub fn untrusted(mut self) -> Self {
        self.source_kind = "tool".into();
        self
    }

    /// Mark the item as soft-deprecated: leak bait.
    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }
}

fn default_source_kind() -> String {
    "user".into()
}

/// One eval case.
#[derive(Debug, Clone, Deserialize)]
pub struct EvalCase {
    /// Stable case id.
    pub id: String,
    /// The query the agent would issue.
    pub query: String,
    /// Memory ids expected to be recalled (relevant set). Must be non-empty.
    pub relevant: Vec<String>,
    /// Memory ids that must NOT be recalled (forgotten, untrusted, deprecated).
    #[serde(default)]
    pub forbidden: Vec<String>,
    /// The candidate corpus for this case: full memories, not just ids.
    pub corpus: Vec<CorpusItem>,
    /// Injection target for this case; `None` → `[recall] top_k`.
    ///
    /// Fixtures set this to `relevant.len()` so `precision` is precision@R.
    #[serde(default)]
    pub k: Option<usize>,
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

/// Reject fixtures that cannot produce a meaningful score.
///
/// Deliberately strict: a case whose corpus is empty, whose `relevant` set is
/// empty, or which references an id it does not define would otherwise score
/// as a silent pass/zero. Duplicated corpus ids are rejected too — a duplicate
/// would dedup on write and make the id → stored-id mapping ambiguous.
pub fn validate_cases(cases: &[EvalCase]) -> Result<()> {
    for case in cases {
        if case.corpus.is_empty() {
            return Err(Error::InvalidInput(format!(
                "eval case '{}': corpus is empty — the case cannot be searched",
                case.id
            )));
        }
        if case.relevant.is_empty() {
            return Err(Error::InvalidInput(format!(
                "eval case '{}': relevant set is empty — nothing to score",
                case.id
            )));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(case.corpus.len());
        for item in &case.corpus {
            if item.text.trim().is_empty() {
                return Err(Error::InvalidInput(format!(
                    "eval case '{}': corpus item '{}' has empty text",
                    case.id, item.id
                )));
            }
            if seen.contains(&item.id.as_str()) {
                return Err(Error::InvalidInput(format!(
                    "eval case '{}': duplicate corpus id '{}'",
                    case.id, item.id
                )));
            }
            seen.push(&item.id);
        }
        for id in case.relevant.iter().chain(case.forbidden.iter()) {
            if !seen.contains(&id.as_str()) {
                return Err(Error::InvalidInput(format!(
                    "eval case '{}': id '{}' is referenced but not in the corpus",
                    case.id, id
                )));
            }
        }
        if let Some(dup) = case.relevant.iter().find(|id| case.forbidden.contains(id)) {
            return Err(Error::InvalidInput(format!(
                "eval case '{}': id '{}' is both relevant and forbidden",
                case.id, dup
            )));
        }
        if case.k == Some(0) {
            return Err(Error::InvalidInput(format!(
                "eval case '{}': k must be > 0",
                case.id
            )));
        }
    }
    Ok(())
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
                "{\"id\":\"c1\",\"query\":\"q\",\"relevant\":[\"m1\"],\"forbidden\":[\"m2\"],",
                "\"corpus\":[{\"id\":\"m1\",\"text\":\"one\"},{\"id\":\"m2\",\"text\":\"two\",\"source_kind\":\"tool\"}]}\n",
                "\n",
                "{\"id\":\"c2\",\"query\":\"q2\",\"relevant\":[\"m3\"],\"corpus\":[",
                "{\"id\":\"m3\",\"text\":\"three\",\"deprecated\":true}],\"k\":1}\n"
            ),
        )
        .unwrap();
        let cases = load_cases(&p).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].relevant, vec!["m1".to_string()]);
        assert_eq!(cases[0].corpus[0].text, "one");
        assert_eq!(cases[0].corpus[0].source_kind, "user");
        assert_eq!(cases[0].corpus[1].source_kind, "tool");
        assert!(cases[1].corpus[0].deprecated);
        assert_eq!(cases[1].k, Some(1));
        validate_cases(&cases).unwrap();
    }

    #[test]
    fn validate_cases_rejects_unscoreable_fixtures() {
        let base = |corpus: Vec<CorpusItem>, relevant: Vec<&str>| EvalCase {
            id: "c".into(),
            query: "q".into(),
            relevant: relevant.into_iter().map(str::to_string).collect(),
            forbidden: vec![],
            corpus,
            k: None,
        };

        // Empty corpus: the silent-zero failure mode of issue 0052.
        let err = validate_cases(&[base(vec![], vec!["m1"])]).unwrap_err();
        assert!(err.to_string().contains("corpus is empty"), "{err}");

        // Empty relevant set: nothing to score.
        let err = validate_cases(&[base(vec![CorpusItem::new("m1", "t")], vec![])]).unwrap_err();
        assert!(err.to_string().contains("relevant set is empty"), "{err}");

        // Relevant id not in the corpus: an unreachable expectation.
        let err =
            validate_cases(&[base(vec![CorpusItem::new("m1", "t")], vec!["m9"])]).unwrap_err();
        assert!(err.to_string().contains("not in the corpus"), "{err}");

        // Duplicate corpus ids would dedup on write.
        let err = validate_cases(&[base(
            vec![CorpusItem::new("m1", "t"), CorpusItem::new("m1", "t2")],
            vec!["m1"],
        )])
        .unwrap_err();
        assert!(err.to_string().contains("duplicate corpus id"), "{err}");

        // Empty corpus text can never be embedded or matched.
        let err =
            validate_cases(&[base(vec![CorpusItem::new("m1", "  ")], vec!["m1"])]).unwrap_err();
        assert!(err.to_string().contains("empty text"), "{err}");

        // A valid case passes.
        validate_cases(&[base(vec![CorpusItem::new("m1", "t")], vec!["m1"])]).unwrap();
    }
}
