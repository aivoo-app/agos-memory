# Observability: cost and token budgets

`agos-memory cost` turns the provider-call ledger into an operator report
without requiring direct SQLite access.

## Commands

```sh
# All recorded calls and recall-budget rows, all time
agos-memory cost

# One conversation session, optionally limited to a recent window
agos-memory cost --session 0f4d9f9e-...
agos-memory cost --since 24h
agos-memory cost --since 7d --json
```

Accepted `--since` suffixes are `ms`, `s`, `m`, `h`, `d`, and `w`. An unknown
session or malformed duration exits with code 2 and an actionable message.
`--json` and human output are rendered from the same `CostReport` object.

The report contains:

- provider calls, failed calls, prompt/completion/total tokens, mean latency,
  and estimated USD;
- one row per purpose (`extract`, `summarize`, `embed`, `eval`);
- aggregated `token_ledger` rows: recall budget, tokens used, items injected,
  and items dropped;
- `[budget] max_tokens_per_session`, remaining headroom, and `over_budget` for a
  session-scoped report.

## What “per session” includes

Schema v5 adds nullable `llm_calls.session_id`. Extraction calls are attributed
to the session whose turns produced them. Existing pre-v5 rows remain
`session_id = NULL`; agos-memory never guesses which session they belonged to.

Sessionless summarize and embedding calls remain visible in the all-time report
but are excluded from `--session` unless the operation itself has real session
context. This is deliberate: false attribution would make a budget report look
precise while charging one conversation for unrelated work.

## Price estimate caveat

`cost_usd_est` is a stable internal estimate of **$0.60 per one million tokens**.
It is not a provider invoice and does not include model-specific input/output
prices, discounts, caching, regional pricing, or agos-proxy accounting. Use
`total_tokens` and `by_purpose` as the authoritative local usage record and the
estimate only for consistent trend comparison.

Local `MockChat` and `HashEmbedder` executions may create zero-cost ledger rows
so hermetic tests exercise the same report. `provider = "none"` creates no
embedding row. Failed provider calls are counted when a failure row exists;
the central success-path recorders do not invent a billable row for a request
that never completed.

## Session ceiling

```toml
[budget]
max_tokens_per_session = 50_000 # 0 disables the ceiling
```

Before additional provider work, the extractor sums `llm_calls.total_tokens`
where `session_id` matches the current session. At or above the configured
ceiling it returns `BudgetExceeded` (CLI exit 8, HTTP 429, MCP invalid params).
`remember` also checks the agent's current open session, but only calls with a
real `session_id` are charged.

This is separate from `[recall] budget_tokens`, which caps injected context per
recall and is recorded in `token_ledger`.
