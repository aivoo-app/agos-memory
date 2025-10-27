# ADR-006: Hybrid Retrieval — In-Scan Filter vs Post-Filter

## Status
Accepted (v0.3.0)

## Context
We need to combine vector KNN (sqlite-vec) and keyword BM25 (FTS5) for recall.
The hard filter (agent_id, tier, status, trust, expiry, supersession) must be enforced
with zero leaks — no disallowed memory may ever reach scoring.

Two architectural approaches were considered:

**A. Post-filter only:** Run both legs unfiltered, fuse, then apply hard filter once.
**B. In-scan filter + post-filter:** Push hard filter *inside* both legs (via sqlite-vec
metadata columns and FTS5 join), then re-apply after fusion.

## Decision
**We choose B: in-scan filter + post-filter.**

## Consequences

### Why in-scan filter?
1. **Correctness (zero leaks):** A candidate filtered out *during* the scan can never
   occupy a top-k slot, so it can never crowd out a valid candidate. Post-filter only
   would allow a filtered candidate to temporarily occupy a slot, potentially
   excluding a valid candidate that would have been in the top-k had the filtered
   one been absent. This is a structural zero-leak guarantee.
2. **Performance:** Pushing metadata filters into sqlite-vec's KNN scan (via
   metadata columns `tier`, `status`, `trust`, `kind`, `pinned`) means the KNN
   search only visits admissible rows. Similarly, the FTS5 leg joins to `memories`
   and filters in the same query. Fewer rows to score/fuse = faster.
3. **Determinism:** The set of candidates reaching fusion is exactly the set
   admitted by the hard filter — no surprises from stale vector metadata.

### Why post-filter *also*?
- **Fail-closed hardening:** sqlite-vec metadata columns can drift from the
  canonical `memories` row (stale vector, deleted memory, concurrent update).
  The post-fusion Rust mirror (`HardFilter::admits`) is the final authority.
  A candidate that passes the SQL predicate but fails the Rust mirror is dropped.
- **Defense in depth:** If a future change to one leg's SQL accidentally weakens
  the predicate, the post-filter catches it. The zero-leak guarantee is structural,
  not dependent on a single query.

### Why RRF fusion after hard filter?
- RRF is rank-based (not score-based), so the two legs' scores never need
  calibrating. The hard filter is applied *before* fusion, so the RRF ranks are
  computed only on admissible candidates.
- A candidate filtered by the hard filter never contributes to RRF, so it
  cannot affect the fused ranking of admissible candidates.

---

## Implementation Notes

- **sqlite-vec metadata columns:** `tier` (TEXT), `status` (INTEGER), `trust` (INTEGER), `kind` (TEXT), `pinned` (INTEGER). Values match the canonical `memories` row encoding.
- **FTS5 join:** `fts_memories` MATCH query joined to `memories` with the same WHERE clause.
- **RRF constant:** `K = 60` (D27). Score = Σ 1/(60 + rank + 1).
- **Rerank pool:** Each leg returns `top_k × RERANK_POOL_FACTOR` (default 4×) candidates so rerank can reorder — fusion alone never decides the cut.

---

## Alternatives Considered

| Option | Pros | Cons |
|--------|------|------|
| Post-filter only | Simpler SQL | Allows filtered candidates to crowd out valid ones; no structural zero-leak guarantee |
| In-scan only | No double filtering | Stale vector metadata could leak disallowed candidates; no fail-closed hardening |
| Score fusion (not rank) | Can weight legs | Requires score calibration; BM25 and cosine are incomparable scales |

---

## Testing

- `tests/recall_filters.rs`: proves SQL predicate and Rust mirror agree row-for-row on every database state (zero leaks across both legs).
- `tests/recall_hybrid.rs`: verifies untrusted excluded, pending excluded by default, degraded mode works.
- `tests/recall_hybrid.rs`: `rrf_fusion_prefers_double_leg_hits` — verifies RRF correctly boosts candidates present in both legs.

---

## Related Decisions
- D6: Hard filter as single source of truth
- D27: RRF fusion (K=60)
- D29: Trust policy (Strict vs Fenced)
- D4: Degraded mode (BM25-only)