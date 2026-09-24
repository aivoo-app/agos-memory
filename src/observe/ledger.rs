//! LLM/provider call ledger and operator-facing cost rollups.
//!
//! Calls carry heuristic token counts and an estimated USD cost. Extraction can
//! be attributed to a conversation session; sessionless calls remain visible in
//! the all-time report with `session_id = NULL`.

use rusqlite::{Connection, ToSql, params_from_iter};

use crate::error::Result;
use crate::storage::StoreHandle;
use crate::util::{TokenCounter, clock::Clock};

/// Purpose of a recorded provider call.
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
    pub fn as_str(&self) -> &'static str {
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
    /// Owning session when the call is attributable.
    pub session_id: Option<i64>,
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

/// Rough list-price estimate used for `cost_usd_est`.
///
/// This is intentionally a stable estimate, not a provider invoice. The report
/// publishes this value alongside every result so operators can interpret it.
pub const USD_PER_1M_TOKENS: f64 = 0.6;

/// Record one synchronous ledger entry.
pub fn record(conn: &Connection, now_millis: i64, entry: &LedgerEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO llm_calls
         (purpose, session_id, model, prompt_tokens, completion_tokens, total_tokens,
          latency_ms, ok, cost_usd_est, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            entry.purpose.as_str(),
            entry.session_id,
            entry.model,
            entry.prompt_tokens as i64,
            entry.completion_tokens as i64,
            (entry.prompt_tokens + entry.completion_tokens) as i64,
            entry.latency_ms as i64,
            if entry.ok { 1 } else { 0 },
            entry.cost_usd_est,
            now_millis
        ],
    )?;
    Ok(())
}

/// Record an entry through the store's serialized writer.
pub async fn record_entry(store: &StoreHandle, entry: &LedgerEntry) -> Result<()> {
    let entry = entry.clone();
    store
        .write(move |conn| {
            let now = crate::util::SystemClock.now_millis();
            record(conn, now, &entry)
        })
        .await
}

/// Build an entry with estimated token counts from text lengths.
pub fn estimated_entry(
    purpose: Purpose,
    model: &str,
    prompt: &str,
    completion: &str,
    latency_ms: u64,
    ok: bool,
) -> LedgerEntry {
    estimated_entry_for_session(purpose, None, model, prompt, completion, latency_ms, ok)
}

/// Same as [`estimated_entry`], with explicit session attribution.
pub fn estimated_entry_for_session(
    purpose: Purpose,
    session_id: Option<i64>,
    model: &str,
    prompt: &str,
    completion: &str,
    latency_ms: u64,
    ok: bool,
) -> LedgerEntry {
    let counter = crate::util::HeuristicCounter::new();
    let prompt_tokens = counter.count(prompt);
    let completion_tokens = counter.count(completion);
    let total = (prompt_tokens + completion_tokens) as f64;
    let mut cost_usd_est = total / 1_000_000.0 * USD_PER_1M_TOKENS;
    if model.starts_with("mock-") || model.starts_with("hash-") {
        // Local mock calls are real executions for tests, but not billable spend.
        cost_usd_est = 0.0;
    }
    LedgerEntry {
        purpose,
        session_id,
        model: model.to_string(),
        prompt_tokens,
        completion_tokens,
        latency_ms,
        ok,
        cost_usd_est,
    }
}

/// Total tokens since `since_millis` across every attributed call.
pub fn tokens_since(conn: &Connection, since_millis: i64) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(total_tokens), 0) FROM llm_calls WHERE created_at >= ?1",
        [since_millis],
        |row| row.get(0),
    )?;
    Ok(total.max(0) as u64)
}

/// Total tokens attributed to one session.
pub fn tokens_for_session(conn: &Connection, session_id: i64) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(total_tokens), 0)
         FROM llm_calls WHERE session_id = ?1",
        [session_id],
        |row| row.get(0),
    )?;
    Ok(total.max(0) as u64)
}

/// Per-purpose provider-call rollup.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct PurposeCost {
    pub purpose: String,
    pub calls: u64,
    pub failed_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub mean_latency_ms: f64,
    pub cost_usd_est: f64,
}

/// Aggregated recall-budget rows in the selected scope/window.
#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct RecallBudgetSummary {
    pub rows: u64,
    pub budget_tokens_total: u64,
    pub tokens_used_total: u64,
    pub items_injected_total: u64,
    pub items_dropped_total: u64,
}

/// Per-session ceiling state; absent from an all-time report.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct SessionBudget {
    pub ceiling_tokens: u64,
    pub used_tokens: u64,
    pub remaining_tokens: Option<u64>,
    pub over_budget: bool,
}

/// Price metadata embedded in every report.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct PricingInfo {
    pub estimated: bool,
    pub usd_per_1m_tokens: f64,
}

/// Complete `agos-memory cost` report.
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct CostReport {
    pub session_id: Option<i64>,
    pub since_millis: i64,
    pub calls: u64,
    pub failed_calls: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub mean_latency_ms: f64,
    pub cost_usd_est: f64,
    pub by_purpose: Vec<PurposeCost>,
    pub recall_budget: RecallBudgetSummary,
    pub max_tokens_per_session: u64,
    pub session_budget: Option<SessionBudget>,
    pub pricing: PricingInfo,
}

fn nonnegative_u64(value: i64) -> u64 {
    value.max(0) as u64
}

fn append_scope(sql: &mut String, params: &mut Vec<Box<dyn ToSql>>, session_id: Option<i64>) {
    if let Some(session_id) = session_id {
        sql.push_str(" AND session_id = ?");
        params.push(Box::new(session_id));
    }
}

/// Query the complete cost report from `llm_calls` and `token_ledger`.
pub fn cost_report(
    conn: &Connection,
    session_id: Option<i64>,
    since_millis: i64,
    max_tokens_per_session: u64,
) -> Result<CostReport> {
    let mut total_sql = String::from(
        "SELECT COUNT(*),
                COALESCE(SUM(CASE WHEN ok = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COALESCE(SUM(total_tokens), 0),
                COALESCE(AVG(latency_ms), 0.0),
                COALESCE(SUM(cost_usd_est), 0.0)
         FROM llm_calls WHERE created_at >= ?1",
    );
    let mut params: Vec<Box<dyn ToSql>> = vec![Box::new(since_millis)];
    append_scope(&mut total_sql, &mut params, session_id);
    let (calls, failed, prompt, completion, total, mean_latency, cost): (
        i64,
        i64,
        i64,
        i64,
        i64,
        f64,
        f64,
    ) = conn.query_row(
        &total_sql,
        params_from_iter(params.iter().map(|p| p.as_ref())),
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;

    let mut purpose_sql = String::from(
        "SELECT purpose, COUNT(*),
                COALESCE(SUM(CASE WHEN ok = 0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(prompt_tokens), 0),
                COALESCE(SUM(completion_tokens), 0),
                COALESCE(SUM(total_tokens), 0),
                COALESCE(AVG(latency_ms), 0.0),
                COALESCE(SUM(cost_usd_est), 0.0)
         FROM llm_calls WHERE created_at >= ?1",
    );
    let mut purpose_params: Vec<Box<dyn ToSql>> = vec![Box::new(since_millis)];
    append_scope(&mut purpose_sql, &mut purpose_params, session_id);
    purpose_sql.push_str(" GROUP BY purpose ORDER BY purpose");
    let mut stmt = conn.prepare(&purpose_sql)?;
    let by_purpose = stmt
        .query_map(
            params_from_iter(purpose_params.iter().map(|p| p.as_ref())),
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, f64>(6)?,
                    row.get::<_, f64>(7)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(
            |(purpose, calls, failed, prompt, completion, total, latency, cost)| PurposeCost {
                purpose,
                calls: nonnegative_u64(calls),
                failed_calls: nonnegative_u64(failed),
                prompt_tokens: nonnegative_u64(prompt),
                completion_tokens: nonnegative_u64(completion),
                total_tokens: nonnegative_u64(total),
                mean_latency_ms: latency,
                cost_usd_est: cost,
            },
        )
        .collect();

    let mut budget_sql = String::from(
        "SELECT COUNT(*), COALESCE(SUM(budget), 0),
                COALESCE(SUM(tokens_used), 0),
                COALESCE(SUM(items_injected), 0),
                COALESCE(SUM(items_dropped), 0)
         FROM token_ledger WHERE created_at >= ?1",
    );
    let mut budget_params: Vec<Box<dyn ToSql>> = vec![Box::new(since_millis)];
    append_scope(&mut budget_sql, &mut budget_params, session_id);
    let (rows, budget, used, injected, dropped): (i64, i64, i64, i64, i64) = conn.query_row(
        &budget_sql,
        params_from_iter(budget_params.iter().map(|p| p.as_ref())),
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;

    let total_tokens = nonnegative_u64(total);
    let session_budget = session_id.map(|_| SessionBudget {
        ceiling_tokens: max_tokens_per_session,
        used_tokens: total_tokens,
        remaining_tokens: (max_tokens_per_session > 0)
            .then(|| max_tokens_per_session.saturating_sub(total_tokens)),
        over_budget: max_tokens_per_session > 0 && total_tokens > max_tokens_per_session,
    });

    Ok(CostReport {
        session_id,
        since_millis,
        calls: nonnegative_u64(calls),
        failed_calls: nonnegative_u64(failed),
        prompt_tokens: nonnegative_u64(prompt),
        completion_tokens: nonnegative_u64(completion),
        total_tokens,
        mean_latency_ms: mean_latency,
        cost_usd_est: cost,
        by_purpose,
        recall_budget: RecallBudgetSummary {
            rows: nonnegative_u64(rows),
            budget_tokens_total: nonnegative_u64(budget),
            tokens_used_total: nonnegative_u64(used),
            items_injected_total: nonnegative_u64(injected),
            items_dropped_total: nonnegative_u64(dropped),
        },
        max_tokens_per_session,
        session_budget,
        pricing: PricingInfo {
            estimated: true,
            usd_per_1m_tokens: USD_PER_1M_TOKENS,
        },
    })
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
                session_id INTEGER,
                model TEXT NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL,
                total_tokens INTEGER NOT NULL,
                latency_ms INTEGER NOT NULL,
                ok INTEGER NOT NULL,
                cost_usd_est REAL NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE token_ledger (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id INTEGER,
                budget INTEGER NOT NULL,
                tokens_used INTEGER NOT NULL,
                items_injected INTEGER NOT NULL,
                items_dropped INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn record_and_sum_tokens_with_session_scope() {
        let conn = setup();
        let mut first = estimated_entry(
            Purpose::Extract,
            "test-model",
            "a prompt of some length",
            "a reply",
            12,
            true,
        );
        first.session_id = Some(7);
        record(&conn, 1000, &first).unwrap();
        let second = estimated_entry(Purpose::Eval, "test-model", "later", "", 4, true);
        record(&conn, 1500, &second).unwrap();

        let second_total = second.total_of();
        assert_eq!(
            tokens_since(&conn, 0).unwrap(),
            first.total_of() + second_total
        );
        assert_eq!(tokens_for_session(&conn, 7).unwrap(), first.total_of());
    }

    impl LedgerEntry {
        fn total_of(&self) -> u64 {
            self.prompt_tokens + self.completion_tokens
        }
    }

    #[test]
    fn report_aggregates_purposes_and_session_budget() {
        let conn = setup();
        let mut first = estimated_entry(
            Purpose::Extract,
            "test-model",
            "a prompt of some length",
            "a reply",
            12,
            true,
        );
        first.session_id = Some(7);
        record(&conn, 1000, &first).unwrap();
        conn.execute(
            "INSERT INTO token_ledger
             (session_id, budget, tokens_used, items_injected, items_dropped, created_at)
             VALUES (7, 100, 40, 3, 1, 1000)",
            [],
        )
        .unwrap();

        let report = cost_report(&conn, Some(7), 0, 9).unwrap();
        assert_eq!(report.calls, 1);
        assert_eq!(report.total_tokens, first.total_of());
        assert_eq!(report.by_purpose[0].purpose, "extract");
        assert_eq!(report.recall_budget.tokens_used_total, 40);
        assert!(report.session_budget.unwrap().over_budget);
    }

    #[test]
    fn mock_cost_is_zero() {
        let entry = estimated_entry(Purpose::Extract, "mock-chat-v1", "prompt", "reply", 1, true);
        assert_eq!(entry.cost_usd_est, 0.0);
    }
}
