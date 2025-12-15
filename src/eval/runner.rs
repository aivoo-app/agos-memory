//! End-to-end eval runner (issue 0052).
//!
//! The harness is only meaningful if it exercises the real path: each case
//! seeds its candidate corpus through the write path (redaction → embedding →
//! dedup → `memories` + `memory_versions` + `vec_memories`, with the FTS5
//! trigger filling `fts_memories`), then runs the real
//! [`crate::recall::recall`] and scores the hits that were actually injected.
//! Nothing is stubbed, and there is no code path that can score an empty
//! database: validation rejects id-only or empty corpora before a single query
//! runs.

use std::collections::HashMap;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::eval::{CaseResult, EvalCase, Metrics};
use crate::memory::extract::Candidate;
use crate::memory::persist::persist_candidate_full;
use crate::recall::{RecallQuery, recall};
use crate::storage::StoreHandle;

/// Extractor version recorded for eval-seeded rows (distinguishable from
/// production extractions in the version chain).
pub const EVAL_EXTRACTOR_VERSION: &str = "eval-v1";

/// One case's scored outcome, kept for reporting and per-case assertions.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseOutcome {
    /// Case id from the fixture.
    pub id: String,
    /// The query text.
    pub query: String,
    /// Fixture ids injected, in injection order.
    pub returned: Vec<String>,
    /// Fixture ids that should have been injected.
    pub relevant: Vec<String>,
    /// Fixture ids that must never be injected.
    pub forbidden: Vec<String>,
    /// Relevant ids that were not injected.
    pub missed: Vec<String>,
    /// Forbidden ids that were injected (must always be empty).
    pub leaked: Vec<String>,
}

impl CaseOutcome {
    /// True when nothing was missed and nothing leaked.
    pub fn perfect(&self) -> bool {
        self.missed.is_empty() && self.leaked.is_empty()
    }

    /// Precision at the injected set (0.0 when nothing was injected).
    pub fn precision(&self) -> f64 {
        if self.returned.is_empty() {
            return 0.0;
        }
        let hits = self
            .returned
            .iter()
            .filter(|id| self.relevant.contains(id))
            .count();
        hits as f64 / self.returned.len() as f64
    }

    /// Recall over the relevant set.
    pub fn recall(&self) -> f64 {
        if self.relevant.is_empty() {
            return 0.0;
        }
        let hits = self
            .relevant
            .iter()
            .filter(|id| self.returned.contains(id))
            .count();
        hits as f64 / self.relevant.len() as f64
    }
}

/// A full eval run: aggregate metrics plus per-case outcomes.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalRun {
    /// Aggregate precision / recall / MRR / leaks.
    pub metrics: Metrics,
    /// Per-case detail, in fixture order.
    pub outcomes: Vec<CaseOutcome>,
}

impl EvalRun {
    /// Cases that missed a relevant memory or leaked a forbidden one.
    pub fn imperfect(&self) -> impl Iterator<Item = &CaseOutcome> {
        self.outcomes.iter().filter(|o| !o.perfect())
    }
}

/// Run every case end-to-end and return metrics + per-case detail.
///
/// Callers should run [`crate::eval::validate_cases`] first; the runner also
/// fails loudly when the corpus cannot be stored (rather than scoring an empty
/// database).
pub async fn run(cfg: &Config, cases: &[EvalCase]) -> Result<EvalRun> {
    let mut outcomes = Vec::with_capacity(cases.len());
    for case in cases {
        outcomes.push(run_case(cfg, case).await?);
    }
    let metrics = Metrics::aggregate(&aggregated_results(&outcomes));
    Ok(EvalRun { metrics, outcomes })
}

/// Run every case and return only the aggregate metrics.
pub async fn run_cases(cfg: &Config, cases: &[EvalCase]) -> Result<Metrics> {
    Ok(run(cfg, cases).await?.metrics)
}

/// Map per-case outcomes onto the scoring form used by [`Metrics::aggregate`].
fn aggregated_results(outcomes: &[CaseOutcome]) -> Vec<CaseResult> {
    outcomes
        .iter()
        .map(|o| CaseResult {
            returned: o.returned.clone(),
            relevant: o.relevant.clone(),
            forbidden: o.forbidden.clone(),
        })
        .collect()
}

/// One case: fresh database → seed the corpus → recall → score.
async fn run_case(cfg: &Config, case: &EvalCase) -> Result<CaseOutcome> {
    let dir =
        tempfile::tempdir().map_err(|e| Error::Storage(format!("eval tempdir failed: {e}")))?;
    let mut case_cfg = cfg.clone();
    case_cfg.db_path = dir.path().join("eval.db");

    let store = StoreHandle::open(&case_cfg, crate::defaults::READ_POOL_SIZE).await?;
    let dim = store.embed_dim().await?;
    let embedder = crate::embed::embedder_from_config(&case_cfg.embed, dim);
    store.validate_embed_dim(&*embedder).await?;

    // Fixture ids are stable and human-written; stored ids are generated, so
    // keep both directions of the mapping. `deduped` must not happen — a
    // corpus that collapses on write would make the mapping ambiguous, and
    // validation already rejects duplicate ids.
    let mut logical_to_public: HashMap<String, String> = HashMap::new();
    for item in &case.corpus {
        let cand = Candidate {
            tier: "semantic".into(),
            kind: "fact".into(),
            text: item.text.clone(),
            importance: 0.5,
            // Above any sane pending threshold: corpus items must be eligible.
            confidence: 1.0,
            session_independent: true,
            source_seq: 0,
        };
        let report = persist_candidate_full(
            &store,
            &cand,
            &*embedder,
            EVAL_EXTRACTOR_VERSION,
            &item.source_kind,
            case_cfg.memory.pending_threshold,
            case_cfg.memory.dedup_threshold,
        )
        .await?;
        if report.deduped {
            return Err(Error::InvalidInput(format!(
                "eval case '{}': corpus item '{}' deduped into an earlier item — \
                 corpus items must be distinct enough to store separately",
                case.id, item.id
            )));
        }
        if item.deprecated {
            store
                .deprecate_memory(report.row.id, Some("eval-fixture"))
                .await?;
        }
        logical_to_public.insert(item.id.clone(), report.row.public_id);
    }

    let public_to_logical: HashMap<&str, &str> = logical_to_public
        .iter()
        .map(|(logical, public)| (public.as_str(), logical.as_str()))
        .collect();

    // `k` is the case's injection target (fixtures use `relevant.len()`), so
    // the cut measures precision@R rather than "everything that fits".
    let mut query = RecallQuery::new(case.query.clone(), &case_cfg.recall);
    query.k = case.k.unwrap_or(case_cfg.recall.top_k).max(1);

    let report = recall(&store, &*embedder, &query).await?;
    let returned: Vec<String> = report
        .hits
        .iter()
        .filter(|h| h.injected())
        .filter_map(|h| public_to_logical.get(h.public_id.as_str()).copied())
        .map(str::to_string)
        .collect();

    let missed = case
        .relevant
        .iter()
        .filter(|id| !returned.contains(id))
        .cloned()
        .collect();
    let leaked = case
        .forbidden
        .iter()
        .filter(|id| returned.contains(id))
        .cloned()
        .collect();

    Ok(CaseOutcome {
        id: case.id.clone(),
        query: case.query.clone(),
        returned,
        relevant: case.relevant.clone(),
        forbidden: case.forbidden.clone(),
        missed,
        leaked,
    })
}
