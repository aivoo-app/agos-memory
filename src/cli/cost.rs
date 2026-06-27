//! `cost` — report provider-call tokens, estimated USD, and recall budget rows.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::memory::sessions;
use crate::observe::ledger::{self, CostReport};
use crate::storage::StoreHandle;
use crate::util::clock::Clock;

/// Run the cost report command.
pub async fn run(
    cfg: &Config,
    session: Option<String>,
    since: Option<String>,
    json: bool,
) -> Result<()> {
    if !cfg.db_path.is_file() {
        return Err(Error::InvalidInput(format!(
            "database {} does not exist — run `agos-memory init` first",
            cfg.db_path.display()
        )));
    }

    let store = StoreHandle::open(cfg, crate::defaults::READ_POOL_SIZE).await?;
    let session_id = match session.as_deref() {
        Some(public_id) => Some(
            sessions::session_id_by_public(&store, public_id)
                .await?
                .ok_or_else(|| Error::InvalidInput(format!("unknown session {public_id}")))?,
        ),
        None => None,
    };
    let since_millis = match since.as_deref() {
        Some(value) => {
            let duration = parse_duration_millis(value)?;
            crate::util::SystemClock
                .now_millis()
                .saturating_sub(duration)
        }
        None => 0,
    };
    let max_tokens_per_session = cfg.budget.max_tokens_per_session;
    let report = store
        .read(move |conn| {
            ledger::cost_report(conn, session_id, since_millis, max_tokens_per_session)
        })
        .await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_human(&report, session.as_deref(), since.as_deref());
    }
    Ok(())
}

fn parse_duration_millis(value: &str) -> Result<i64> {
    let value = value.trim();
    let (digits, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1_i64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else if let Some(value) = value.strip_suffix('d') {
        (value, 86_400_000)
    } else if let Some(value) = value.strip_suffix('w') {
        (value, 604_800_000)
    } else {
        return Err(Error::InvalidInput(
            "--since must be a positive duration like 30m, 24h, 7d, or 2w".into(),
        ));
    };
    let amount: i64 = digits.parse().map_err(|_| {
        Error::InvalidInput(format!(
            "--since has an invalid duration `{value}`; use 30m, 24h, 7d, or 2w"
        ))
    })?;
    if amount <= 0 {
        return Err(Error::InvalidInput("--since duration must be > 0".into()));
    }
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| Error::InvalidInput("--since duration is too large".into()))
}

fn print_human(report: &CostReport, session: Option<&str>, since: Option<&str>) {
    println!(
        "scope:      {}",
        session.map_or_else(|| "all sessions".to_string(), |id| format!("session {id}"))
    );
    println!(
        "window:     {}",
        since.map_or_else(|| "all time".to_string(), |value| format!("last {value}"))
    );
    println!("calls:      {}", report.calls);
    println!("failed:     {}", report.failed_calls);
    println!("prompt:     {} tokens", report.prompt_tokens);
    println!("completion: {} tokens", report.completion_tokens);
    println!("tokens:     {}", report.total_tokens);
    println!("latency:    {:.2} ms mean", report.mean_latency_ms);
    println!("cost:       ${:.8} estimated", report.cost_usd_est);
    println!("by purpose:");
    if report.by_purpose.is_empty() {
        println!("  (none)");
    } else {
        for purpose in &report.by_purpose {
            println!(
                "  {:<10} calls={} prompt={} completion={} total={} mean={:.2}ms cost=${:.8}",
                purpose.purpose,
                purpose.calls,
                purpose.prompt_tokens,
                purpose.completion_tokens,
                purpose.total_tokens,
                purpose.mean_latency_ms,
                purpose.cost_usd_est
            );
        }
    }
    println!(
        "recall:     rows={} budget={} used={} injected={} dropped={}",
        report.recall_budget.rows,
        report.recall_budget.budget_tokens_total,
        report.recall_budget.tokens_used_total,
        report.recall_budget.items_injected_total,
        report.recall_budget.items_dropped_total
    );
    if let Some(budget) = &report.session_budget {
        match budget.remaining_tokens {
            Some(remaining) => println!(
                "budget:     {}/{} tokens used, {remaining} remaining{}",
                budget.used_tokens,
                budget.ceiling_tokens,
                if budget.over_budget {
                    " — OVER BUDGET"
                } else {
                    ""
                }
            ),
            None => println!("budget:     unlimited (ceiling is 0)"),
        }
    } else {
        println!(
            "ceiling:    {} tokens per session (total scope is not compared)",
            report.max_tokens_per_session
        );
    }
    println!(
        "pricing:    estimated at ${:.2}/1M tokens (not a provider invoice)",
        report.pricing.usd_per_1m_tokens
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_suffixes_parse() {
        assert_eq!(parse_duration_millis("30m").unwrap(), 1_800_000);
        assert_eq!(parse_duration_millis("24h").unwrap(), 86_400_000);
        assert_eq!(parse_duration_millis("7d").unwrap(), 604_800_000);
        assert_eq!(parse_duration_millis("2w").unwrap(), 1_209_600_000);
    }

    #[test]
    fn invalid_durations_are_actionable() {
        let err = parse_duration_millis("later").unwrap_err().to_string();
        assert!(err.contains("30m"), "{err}");
        assert!(parse_duration_millis("0d").is_err());
    }
}
