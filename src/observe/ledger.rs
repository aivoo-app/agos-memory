//! LLM call ledger (issue 0011).
//!
//! Every LLM/embed call is recorded so the nightly report can answer
//! "what did memory cost this week?" and the per-session token ceiling
//! can be enforced (§6 of the plan). Cost is *estimated* at list prices
//! and clearly surfaced as an estimate.

use rusqlite::Connection;

use crate::error::Result;
use crate::util::TokenCounter;

/// Purpose of a recorded LLM call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Memory extraction from a turn.
    Extract,
    /// Session/maintenance summarization.
    Summarize,
    /// Embedding computation.
    Embed,
    /// Eval harness calls.
    Eval,
}

impl Purpose {
    fn as_str(&self) -> &'static str {
        match self {
            Purpose::Extract => "extract",
            Purpose::Summarize => "summarize",
            Purpose::Embed => "embed",
            Purpose::Eval => "eval",
        }
    }
}

/// One record for the `llm_calls` table.
#[derive(Debug, Clone)]
pub struct LedgerEntry {
    /// Call purpose.
    pub purpose: Purpose,
    /// Model name.
    pub model: String,
    /// Prompt tokens (estimated or reported).
    pub prompt_tokens: u64,
    /// Completion tokens (estimated or reported).
    pub completion_tokens: u64,
    /// Round-trip latency in milliseconds.
    pub latency_ms: u64,
    /// Whether the call succeeded.
    pub ok: bool,
    /// Rough USD cost estimate.
    pub cost_usd_est: f64,
}

/// Rough list-price estimate used for `cost_usd_est`. Real per-provider
/// pricing arrives with the agos-proxy usage integration (v0.5.0); this keeps
/// the ledger schema and queries stable from day one.
const USD_PER_1M_TOKENS: f64 = 0.6;

/// Record one call into `llm_calls`.
pub fn record(conn: &Connection, now_millis: i64, e: &LedgerEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_calls (purpose, model, prompt_tokens, completion_tokens,
                                total_tokens, latency_ms, ok, cost_usd_est, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            e.purpose.as_str(),
            e.model,
            e.prompt_tokens as i64,
            e.completion_tokens as i64,
            (e.prompt_tokens + e.completion_tokens) as i64,
            e.latency_ms as i64,
            if e.ok { 1 } else { 0 },
            e.cost_usd_est,
            now_millis
        ],
    )?;
    Ok(())
}

/// Helper: build an entry with estimated token counts from text lengths.
pub fn estimated_entry(
    purpose: Purpose,
    model: &str,
    prompt: &str,
    completion: &str,
    latency_ms: u64,
    ok: bool,
) -> LedgerEntry {
    let c = crate::util::HeuristicCounter::new();
    let prompt_tokens = c.count(prompt);
    let completion_tokens = c.count(completion);
    let total = (prompt_tokens + completion_tokens) as f64;
    LedgerEntry {
        purpose,
        model: model.to_string(),
        prompt_tokens,
        completion_tokens,
        latency_ms,
        ok,
        cost_usd_est: total / 1_000_000.0 * USD_PER_1M_TOKENS,
    }
}

/// Total tokens used since `since_millis` (for the session ceiling check).
pub fn tokens_since(conn: &Connection, since_millis: i64) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(total_tokens), 0) FROM llm_calls WHERE created_at >= ?1",
        [since_millis],
        |r| r.get(0),
    )?;
    Ok(total.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE llm_calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                purpose TEXT NOT NULL,
                model TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                ok INTEGER NOT NULL,
                cost_usd_est REAL NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn record_and_sum_tokens() {
        let conn = setup();
        let e = estimated_entry(
            Purpose::Extract,
            "m",
            "a prompt of some length",
            "a reply",
            12,
            true,
        );
        record(&conn, 1000, &e).unwrap();
        record(&conn, 1500, &e).unwrap();

        let since_all = tokens_since(&conn, 0).unwrap();
        assert_eq!(since_all, e.total_of() * 2);
        let since_late = tokens_since(&conn, 1200).unwrap();
        assert_eq!(since_late, e.total_of());
    }

    impl LedgerEntry {
        fn total_of(&self) -> u64 {
            self.prompt_tokens + self.completion_tokens
        }
    }

    #[test]
    fn estimate_overestimates_cost_floor() {
        let e = estimated_entry(
            Purpose::Eval,
            "m",
            &"x".repeat(4000),
            &"y".repeat(1000),
            5,
            true,
        );
        assert!(e.cost_usd_est > 0.0);
        assert_eq!(e.purpose.as_str(), "eval");
    }
}
