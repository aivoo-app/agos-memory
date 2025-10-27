# Recall (Read Path) — Technical Specification

This document specifies the agos-memory recall path (v0.3.0) in full mathematical detail.

## Overview

The recall path answers the question: *Given a query text, which stored memories are most relevant?*

It is a three-stage pipeline:

1. **Candidate generation** — hybrid retrieval (vector + keyword)
2. **Hard filter** — structural guarantees on eligibility
3. **Rerank + pack** — score, order, and fit into a token budget

All decisions are deterministic, auditable, and testable offline.

---

## 1. Candidate Generation (Dual-Leg Retrieval)

Two independent legs produce candidate sets that are fused by Reciprocal Rank Fusion (RRF).

### 1.1 Vector Leg (KNN via sqlite-vec)

- Embedding: query text → vector via configured `Embedder`
- Index: `vec_memories` virtual table (sqlite-vec `vec0`, cosine distance)
- Metadata filter: pushed *inside* the KNN scan via `vec0` metadata columns (`tier`, `status`, `trust`, `kind`, `pinned`)
- Output: top `k × RRF_POOL_FACTOR` rows with cosine distances

### 1.2 Keyword Leg (BM25 via FTS5)

- Tokenization: FTS5 default (Unicode-61, case-insensitive)
- Query: bare terms ANDed (every term must appear)
- Index: `fts_memories` external-content table over `memories.text` with triggers
- Join: inner join to `memories` for hard-filterable columns (`tier`, `status`, `trust`)
- Output: top `k × RRF_POOL_FACTOR` rows with BM25 scores

### 1.3 Fusion: Reciprocal Rank Fusion (RRF)

RRF constant `K = 60` (D27). For each leg, rank `r` (0-based) contributes `1/(K + r + 1)`.

```
score(m) = Σ_legs 1/(K + rank_leg(m) + 1)
```

- Vector leg: rank by cosine distance (smaller = better)
- Keyword leg: rank by BM25 score (larger = better)
- Fusion is rank-based → no score calibration needed
- Ties broken deterministically by `public_id`

**Crucial invariant (D27):** The hard filter is *re-applied* after fusion. No candidate bypasses the structural zero-leak guarantee.

---

## 2. Hard Filter — Single Source of Truth (D6, D29)

The filter defines "what may reach scoring." It exists in two forms that must agree:

### 2.1 SQL Predicate (`HardFilter::predicate`)

Evaluated by SQLite inside both legs and in post-fusion verification.

```sql
WHERE m.agent_id = ?1
  AND m.tier IN (<eligible_tiers>)
  AND m.status IN (<eligible_statuses>)
  AND m.trust IN (<eligible_trusts>)
  AND (m.expires_at IS NULL OR m.expires_at > ?2)
  AND m.superseded_by_id IS NULL
  AND (SELECT s.id FROM memories s WHERE s.supersedes_id = m.id LIMIT 1) IS NULL
```

- `?1` = `agent_id`, `?2` = `now_millis`
- `eligible_tiers`: `working`, `semantic`, `procedural` (+ `episodic` if opted in)
- `eligible_statuses`: `active` (+ `pending` if opted in)
- `eligible_trusts`: `trusted`, `system` (+ `untrusted` only under `TrustPolicy::Fenced`)

### 2.2 Rust Mirror (`HardFilter::admits`)

Evaluated in Rust on resolved `CanonicalRow` — the *fail-closed* mirror.

```rust
fn admits(&self, row: &CanonicalRow) -> bool {
    row.agent_id == self.agent_id
        && self.eligible_tiers.contains(&row.tier)
        && self.eligible_statuses.contains(&row.status)
        && self.eligible_trusts.contains(&row.trust)
        && row.expires_at.map_or(true, |exp| exp > self.now)
        && row.superseded_by_id.is_none()
        && row.successor_id.is_none()  // hardening: exclude anything pointed at by a successor
}
```

**Test:** `tests/recall_filters.rs` proves SQL and Rust predicates agree row-for-row on every database state.

---

## 3. Rerank (D23, D24)

After fusion and re-filtering, candidates are rescored by a blended formula:

```
rerank_score = w1 · sim_norm + w2 · importance_eff + w3 · decay
```

Where:

- `sim_norm` — RRF score normalized to [0,1] by its theoretical ceiling (perfect RRF)
- `importance_eff` = `importance_current × (confidence if status == 'pending' else 1.0)`
- `decay` = per-tier half-life: `0.5^(age_hours / half_life_hours)`

Default weights (D23): `w1 = 0.60`, `w2 = 0.25`, `w3 = 0.15`

Default half-lives (D24):
- `working`: 6 hours
- `episodic`: 21 days (504 hours)
- `semantic`: ∞ (no decay)
- `procedural`: ∞

**Ordering:** descending `rerank_score`, tie-broken by `public_id`.

**Cuts:** after rerank, keep top `k`, then apply `min_score` threshold (D26). `min_score` default: `0.35`.

---

## 4. Token Packing (D25)

After rerank cuts, items are packed into a token budget (`budget_tokens`, default 1500) with tier shares:

| Tier | Share |
|------|-------|
| working | 40% |
| episodic | 30% |
| semantic | 20% |
| procedural | 10% |

**Rules (D25):**

1. **Pinned first** — claimed budget before tier shares; not bound by tier share.
2. **Tier shares with rollover** — offered in declared order (working → episodic → semantic → procedural); unused share rolls down.
3. **Whole-item drop or summary-swap** — never truncate. If full text doesn't fit, use stored `summary_text` (with `summary_tokens` cost) if it fits; otherwise drop the whole item.
4. **Ceiling invariant** — running total `tokens_used ≤ budget_tokens` enforced on every placement.

**Output:** `RecallReport` with `hits` in injection order (pinned first, then by rerank score within tier), each hit tagged with `Placement` (`Full`, `Summary`, `Dropped(TierBudget|TotalBudget|Unresolved)`).

---

## 5. No-Hit Semantics (D26)

If no item survives `min_score` / packing cuts:

```rust
RecallReport {
    hits: vec![],
    tokens_used: 0,
    tier_tokens: TierTokens::default(),
    no_hit: true,
    degraded: false, // or true if vector leg was skipped
    latency_ms: ...,
}
```

- **Never a weak hit** — the caller decides what to do with an empty injection.
- Wire format (D26 phrasing): `No useful memories for "<query>" (min_score=<x>, candidates=<n>)`

---

## 6. Trust Policy (D29)

| Policy | Eligible Trust | Untrusted Handling |
|--------|----------------|-------------------|
| `Strict` (default) | `trusted`, `system` | Excluded from candidate set |
| `Fenced` | `trusted`, `system`, `untrusted` | Retrieved but rendered in fence: `<memory trust="untrusted">...</memory>` |

**Pinning does not launder provenance** — a pinned untrusted memory is still excluded under `Strict`.

---

## 7. Audit Trail (0036)

Every `recall()` call writes three linked rows (single transaction):

| Table | Columns |
|-------|---------|
| `recalls` | `query_hash`, `query_text`, `k`, `budget`, `candidates`, `injected`, `dropped`, `top_score`, `no_hit`, `degraded`, `latency_ms`, `session_id`, `created_at` |
| `recall_items` | `recall_id`, `memory_id`, `rank`, `score`, `components_json`, `injected` |
| `token_ledger` | `session_id`, `budget`, `tokens_used`, `tier_split_json`, `items_injected`, `items_dropped`, `created_at` |

- `session_id` nullable (CLI recall is sessionless)
- `components_json` = serialized `RecallComponents` (RRF, BM25, sim, importance, confidence, decay)
- `tier_split_json` = serialized `TierTokens`
- Best-effort: audit failure is logged but never fails the recall call.

---

## 8. Explain / Citations (0035)

### 8.1 `MemoryExplain`

Full lineage for one memory (agent-scoped):

- Provenance: `source_kind`, `source_ref`
- Version chain: `supersedes` / `superseded_by`
- Graph links: `memory_links` edges
- `ref_count` (read-path usage)
- Recall injections: `(recall_id, rank, score, injected)` from `recall_items`

### 8.2 `why(hit)` — Rank Boosters

Human-readable list of signals that *actually contributed* to the rerank score:

- `"vector"` — leg contributed to RRF
- `"bm25"` — keyword leg contributed
- `"important"` — `importance_eff ≥ 0.7`
- `"fresh"` — `decay ≥ 0.9`
- `"in-budget"` — item was injected

**Invariant:** an explanation never names a signal that didn't numerically contribute.

### 8.3 Wire Format (D29)

Injected memories rendered as fenced blocks:

```xml
<memory id="pid-abc123" tier="semantic" trust="trusted" score="0.8421">
  memory text here...
</memory>
```

Untrusted memories under `Fenced` policy carry `trust="untrusted"` so the consumer treats them as data, never instructions.

---

## 9. Degraded Mode (D4)

When embedder refuses (unavailable / ceiling hit):

- Vector leg skipped entirely
- Keyword leg runs normally
- `RecallReport.degraded = true`
- Hard filter still applies
- No panic, no degraded quality assertion — just explicit flag

---

## 10. Determinism Guarantees

- Same query + same store state → identical `RecallReport` (including item order)
- `HashEmbedder` provides deterministic embeddings for offline eval
- All timestamps from injected `Clock` trait (testable with `FakeClock`)
- Criterion benchmarks use `HashEmbedder` for hermetic, reproducible runs

---

## 10. Appendix: Decision References

| Decision | Document |
|----------|----------|
| D6 | Hard filter single source of truth |
| D13 | Trust policy (trusted/untrusted/system) |
| D23 | Rerank weight blend |
| D24 | Per-tier half-life decay |
| D25 | Token packing rules |
| D26 | No-hit semantics |
| D27 | RRF fusion (K=60) |
| D28 | Default k=8, pending opt-in |
| D29 | Trust policy & fencing |
| D4 | Degraded mode (keyword-only fallback) |