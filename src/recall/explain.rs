//! Explain, citations, and the no-hit path (0035, D26).
//!
//! Three deliverables live here:
//!
//! - [`RecallItem`] — the consumer-facing projection of a [`RecallHit`]:
//!   id, score, the full component breakdown, trust, provenance, and what
//!   packing decided ([`RecallHit::injected`]/[`RecallHit::is_summary`]).
//! - [`why`] — the list of signals that boosted a hit's rank, mirroring the
//!   D23 blend terms so an explanation can never name a signal that did not
//!   contribute.
//! - [`render`] — the CLI/MCP wire format: fenced `<memory>` blocks (untrusted
//!   content carries `trust="untrusted"` so the consumer treats it as data),
//!   and the D26 no-hit line.
//!
//! [`RecallReport::no_hit`] is set by the orchestrator (D26): when nothing
//! scored at or above `min_score`, the report is empty and flagged — the
//! caller decides, recall never panics.
//!
//! [`RecallHit`]: super::fuse::RecallHit
//! [`RecallReport::no_hit`]: super::fuse::RecallReport::structfield.no_hit

use std::collections::HashMap;

use crate::error::Result;
use crate::storage::StoreHandle;

use rusqlite::OptionalExtension;

use super::fuse::{RecallComponents, RecallHit, RecallReport};

/// Consumer-facing view of one recall hit (0035 output contract).
#[derive(Debug, Clone, PartialEq)]
pub struct RecallItem {
    /// Public (external) id of the memory.
    pub public_id: String,
    /// Tier of the memory.
    pub tier: String,
    /// Final rerank score (D23).
    pub score: f64,
    /// Full component breakdown (D23 terms) — the explain payload.
    pub components: RecallComponents,
    /// Provenance trust; untrusted items are rendered fenced (D29).
    pub trust: String,
    /// Provenance kind (`user|agent|tool|file|web|import`).
    pub source_kind: String,
    /// Provenance reference (path, url, …), when one was recorded.
    pub source_ref: Option<String>,
    /// True when packing placed this item into the context (D25).
    pub injected: bool,
    /// True when the summary was injected instead of the full text (D25).
    pub summary: bool,
}

impl RecallHit {
    /// Project this hit into the consumer-facing [`RecallItem`] view.
    pub fn to_item(&self) -> RecallItem {
        RecallItem {
            public_id: self.public_id.clone(),
            tier: self.tier.clone(),
            score: self.score,
            components: self.components.clone(),
            trust: self.trust.clone(),
            source_kind: self.source_kind.clone(),
            source_ref: self.source_ref.clone(),
            injected: self.injected(),
            summary: self.is_summary(),
        }
    }
}

impl RecallReport {
    /// The consumer-facing item view of every retained hit, in report
    /// (injection) order. Empty exactly when [`RecallReport::no_hit`] is set.
    ///
    /// [`RecallReport::no_hit`]: super::fuse::RecallReport::structfield.no_hit
    pub fn items(&self) -> Vec<RecallItem> {
        self.hits.iter().map(RecallHit::to_item).collect()
    }
}

/// The signals that boosted this hit's rank (0035: the `why` list).
///
/// Every key names a term that actually contributed to the D23 score — an
/// explanation may never claim a signal the numbers do not back.
pub fn why(hit: &RecallHit) -> Vec<&'static str> {
    let mut out = Vec::new();
    if hit.components.sim.is_some() {
        out.push("vector");
    }
    if hit.components.bm25_rank.is_some() {
        out.push("bm25");
    }
    if hit.components.importance >= 0.7 {
        out.push("important");
    }
    if hit.components.decay >= 0.9 {
        out.push("fresh");
    }
    if hit.injected() {
        out.push("in-budget");
    }
    out
}

/// Render one injected hit as a fenced `<memory>` block (0035 wire format).
///
/// `text` is the injected text — the full text, or the summary when
/// [`RecallHit::is_summary`] (D25 summary-swap). Untrusted content keeps the
/// same fence but carries `trust="untrusted"`, so the consuming agent treats
/// it as data, never as instructions (D29).
pub fn render_item(hit: &RecallHit, text: &str) -> String {
    format!(
        "<memory id=\"{}\" tier=\"{}\" trust=\"{}\" score=\"{:.4}\">{}</memory>",
        hit.public_id, hit.tier, hit.trust, hit.score, text,
    )
}

/// Render the no-hit outcome (D26): say why, count what was considered.
///
/// `query` is the original query text, included verbatim (quoted) so the
/// consumer can show exactly what was asked.
pub fn render_no_hit(query: &str, min_score: f64, candidates: usize) -> String {
    format!("No useful memories for \"{query}\" (min_score={min_score}, candidates={candidates})")
}

/// The `why(memory_id)` payload (0035): everything the store knows about one
/// memory's life, provenance, and usage.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryExplain {
    /// Public id.
    pub public_id: String,
    /// Provenance.
    pub source_kind: String,
    /// Provenance reference, when one was recorded.
    pub source_ref: Option<String>,
    /// Row this memory replaced, by public id.
    pub supersedes: Option<String>,
    /// Row that replaced this memory, by public id.
    pub superseded_by: Option<String>,
    /// Outgoing `memory_links` edges: `(kind, to_public_id)`.
    pub links: Vec<(String, String)>,
    /// Reference counter (read-path usage).
    pub ref_count: i64,
    /// Recall calls that injected this memory, newest first:
    /// `(recall_id, rank, score, injected)` — via `recall_items`.
    pub recalls: Vec<(i64, i64, f64, bool)>,
}

/// Load the [`MemoryExplain`] for `public_id`, or `None` when the id is
/// unknown (or belongs to another agent — the query is agent-scoped).
pub async fn explain(
    store: &StoreHandle,
    agent_id: &str,
    public_id: &str,
) -> Result<Option<MemoryExplain>> {
    let agent_id = agent_id.to_string();
    let public_id = public_id.to_string();
    let public_id_clone = public_id.clone();

    // Provenance + version chain + ref_count in one row; agent-scoped.
    let base = store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id, m.source_kind, m.source_ref, m.ref_count,
                        sup.public_id, sub.public_id
                 FROM memories m
                 LEFT JOIN memories sup ON sup.id = m.supersedes_id
                 LEFT JOIN memories sub ON sub.id = m.superseded_by_id
                 WHERE m.public_id = ?1 AND m.agent_id = ?2",
            )?;
            stmt.query_row(rusqlite::params![public_id_clone, agent_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .optional()
            .map_err(|e| crate::error::Error::Storage(e.to_string()))
        })
        .await?;
    let Some((id, source_kind, source_ref, ref_count, supersedes, superseded_by)) = base else {
        return Ok(None);
    };

    // Links + recall injections.
    let (links, recalls) = store
        .read(move |conn| {
            let mut links = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT l.kind, p.public_id
                     FROM memory_links l
                     JOIN memories p ON p.id = l.to_memory_id
                     WHERE l.from_memory_id = ?1
                     ORDER BY l.id",
                )?;
                let mapped = stmt.query_map([id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?;
                for row in mapped {
                    links.push(row?);
                }
            }
            let mut recalls = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT ri.recall_id, ri.rank, ri.score, ri.injected
                     FROM recall_items ri
                     WHERE ri.memory_id = ?1
                     ORDER BY ri.recall_id DESC, ri.rank",
                )?;
                let mapped = stmt.query_map([id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, bool>(3)?,
                    ))
                })?;
                for row in mapped {
                    recalls.push(row?);
                }
            }
            Ok((links, recalls))
        })
        .await?;

    Ok(Some(MemoryExplain {
        public_id: public_id.to_string(),
        source_kind,
        source_ref,
        supersedes,
        superseded_by,
        links,
        ref_count,
        recalls,
    }))
}

/// Convenience: render a whole report — injected `<memory>` blocks in
/// injection order, or the D26 no-hit line. `texts` maps public ids to the
/// injected text (full or summary); unlisted ids are skipped (dropped hits).
pub fn render_report(
    report: &RecallReport,
    texts: &HashMap<String, String>,
    min_score: f64,
) -> String {
    if report.no_hit {
        return render_no_hit("", min_score, report.hits.len());
    }
    let mut out = String::new();
    for hit in &report.hits {
        if let Some(text) = texts.get(&hit.public_id) {
            out.push_str(&render_item(hit, text));
            out.push('\n');
        }
    }
    out
}
