//! Summarization (issue 0041).
//!
//! Two modes of summarization:
//! - **Periodic**: background job picks memories older than `summarize_after_days`
//!   (configurable per tier), generates summary via LLM, stores as `summary_text`
//!   + `summary_tokens`.
//! - **On-demand**: `summarize --id <id>` or `summarize --tier <tier>` CLI commands.
//!
//! Summary is stored in the memory row (`summary_text`, `summary_tokens`).
//! The original text remains immutable in `memory_versions`. Recall packing uses
//! `summary_text` + `summary_tokens` for summary-swap (D25).

use crate::config::Config;
use crate::error::Result;
use crate::llm::ChatClient;
use crate::storage::{MemoryRow, StoreHandle};
use crate::util::clock::Clock;
use crate::util::tokens::HeuristicCounter;
use crate::util::tokens::TokenCounter;
use std::sync::Arc;

/// Summarization prompt for the LLM.
const SUMMARIZATION_PROMPT: &str = r#"You are a precise summarization engine. Given a memory text, produce a concise summary that preserves the key facts, entities, and actionable information. The summary must be self-contained and usable as a standalone replacement for the original text when space is limited.

Rules:
- Preserve all specific facts, dates, numbers, names, and decisions.
- Preserve actionable instructions or commitments.
- Remove conversational filler, hedging, and redundancy.
- Do not add information not present in the original.
- Target length: 1/3 to 1/2 of original token count, but never exceed 500 tokens.
- Output ONLY the summary text, no preamble or formatting."#;

/// Outcome of a summarization operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SummarizeReport {
    /// The memory row that was summarized.
    pub memory: MemoryRow,
    /// The generated summary text.
    pub summary_text: String,
    /// Token count of the summary (measured by the same heuristic used for budgeting).
    pub summary_tokens: i64,
    /// ROUGE-L F1 (issue 0049) of the summary against the memory's source
    /// text — compression fidelity: how much of the original's token
    /// structure the summary preserved. 1.0 = verbatim copy, 0.0 = disjoint.
    pub quality_score: f64,
    /// Whether an existing summary was overwritten.
    pub overwrote: bool,
}

/// Summarize a single memory by ID.
pub async fn summarize_by_id(
    store: &StoreHandle,
    llm: &Arc<dyn ChatClient>,
    _config: &Config,
    memory_id: i64,
    force: bool,
) -> Result<SummarizeReport> {
    // Fetch the memory
    let memory = store.get_memory_by_id(memory_id).await?.ok_or_else(|| {
        crate::error::Error::InvalidInput(format!("memory {} not found", memory_id))
    })?;

    let overwrote = memory.summary_text.is_some();
    // Check if already has summary
    if !force && overwrote {
        return Err(crate::error::Error::InvalidInput(
            "memory already has a summary; use --force to overwrite".to_string(),
        ));
    }

    // Generate summary via LLM
    let summary_text = generate_summary(llm, &memory.text).await?;
    let summary_tokens = count_tokens(&summary_text);
    let summary_text_for_update = summary_text.clone();

    // Update the memory row with summary
    store
        .write(move |conn| {
            let now = crate::util::SystemClock.now_millis();
            conn.execute(
                "UPDATE memories SET summary_text = ?1, summary_tokens = ?2, updated_at = ?3 WHERE id = ?4",
                rusqlite::params![&summary_text_for_update, summary_tokens as i64, now, memory.id],
            )?;
            Ok(())
        })
        .await?;
    // After update, fetch the memory by public_id since we have the public_id
    let updated_memory = store
        .get_memory(memory.public_id.clone())
        .await?
        .ok_or_else(|| {
            crate::error::Error::InvalidInput("memory not found after update".to_string())
        })?;

    let quality_score = crate::eval::rouge_l(&memory.text, &summary_text);

    Ok(SummarizeReport {
        memory: updated_memory,
        summary_text,
        summary_tokens,
        quality_score,
        overwrote,
    })
}

/// Summarize all memories in a tier that don't have a summary and are older than `summarize_after_days`.
pub async fn summarize_tier(
    store: &StoreHandle,
    llm: &Arc<dyn ChatClient>,
    config: &Config,
    tier: &str,
) -> Result<Vec<SummarizeReport>> {
    let summarize_after_days = config.memory.summarize_after_days_for_tier(tier);
    if summarize_after_days == 0 {
        return Ok(vec![]); // disabled for this tier
    }

    let cutoff =
        crate::util::SystemClock.now_millis() - (summarize_after_days as i64 * 24 * 60 * 60 * 1000);
    let tier_owned = tier.to_string();

    // Find memories without summary, older than cutoff, not deprecated
    let memories: Vec<MemoryRow> = store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, public_id, tier, kind, text, status, trust, created_at, updated_at,
                        summary_text, summary_tokens
                 FROM memories
                 WHERE tier = ?1
                   AND summary_text IS NULL
                   AND status != 'deleted'
                   AND created_at < ?2
                 ORDER BY created_at ASC",
            )?;
            let rows = {
                let iter = stmt.query_map(rusqlite::params![&tier_owned, cutoff], |r| {
                    Ok(crate::storage::MemoryRow {
                        id: r.get(0)?,
                        public_id: r.get(1)?,
                        tier: r.get(2)?,
                        kind: r.get(3)?,
                        text: r.get(4)?,
                        status: r.get(5)?,
                        trust: r.get(6)?,
                        created_at: r.get(7)?,
                        updated_at: r.get(8)?,
                        summary_text: r.get(9)?,
                        summary_tokens: r.get(10).unwrap_or(0),
                    })
                })?;
                let mut rows = Vec::new();
                for row in iter {
                    rows.push(row.map_err(|e| crate::error::Error::Storage(e.to_string()))?);
                }
                rows
            };
            Ok(rows)
        })
        .await?;

    let mut reports = Vec::new();
    for memory in memories {
        let summary_text = generate_summary(llm, &memory.text).await?;
        let summary_tokens = count_tokens(&summary_text);
        let summary_text_for_update = summary_text.clone();

        store
            .write(move |conn| {
                let now = crate::util::SystemClock.now_millis();
                conn.execute(
                    "UPDATE memories SET summary_text = ?1, summary_tokens = ?2, updated_at = ?3 WHERE id = ?4",
                    rusqlite::params![&summary_text_for_update, summary_tokens as i64, now, memory.id],
                )?;
                Ok(())
            })
            .await?;
        let updated_memory = store
            .get_memory(memory.public_id.clone())
            .await?
            .ok_or_else(|| {
                crate::error::Error::InvalidInput("memory not found after update".to_string())
            })?;

        reports.push(SummarizeReport {
            memory: updated_memory,
            quality_score: crate::eval::rouge_l(&memory.text, &summary_text),
            summary_text,
            summary_tokens,
            overwrote: false,
        });
    }

    Ok(reports)
}

/// Generate a summary using the LLM.
async fn generate_summary(llm: &Arc<dyn ChatClient>, text: &str) -> Result<String> {
    let prompt = format!("{}\n\nText to summarize:\n{}", SUMMARIZATION_PROMPT, text);
    let response = llm.complete(&prompt).await?;
    Ok(response.trim().to_string())
}

/// Count tokens using the same heuristic as the recall budget.
fn count_tokens(text: &str) -> i64 {
    let counter = HeuristicCounter::new();
    TokenCounter::count(&counter, text) as i64
}

/// Periodic summarization job (run by worker).
pub async fn run_summarization_job(
    store: &StoreHandle,
    llm: &Arc<dyn ChatClient>,
    config: &Config,
) -> Result<u64> {
    let mut total = 0;
    for tier in ["working", "episodic", "semantic", "procedural"] {
        let reports = summarize_tier(store, llm, config, tier).await?;
        total += reports.len() as u64;
    }
    Ok(total)
}
