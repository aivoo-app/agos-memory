# ADR-007: Token Packing — Whole-Item Drop vs Truncation

## Status
Accepted (v0.3.0)

## Context
After rerank, we must fit the selected memories into a token budget (`budget_tokens`, default 1500).
Three strategies were considered for items that don't fit:

**A. Truncate:** Cut the text at the token boundary, append `...`.
**B. Summary-swap:** If the item has a stored `summary_text` that fits, inject that instead; otherwise drop the item whole.
**C. Whole-item drop:** If the full text doesn't fit, drop the item entirely (no summary attempt).

## Decision
**We choose B: Summary-swap with whole-item drop as fallback.**

## Rationale

### Why not truncate (A)?
A truncated memory injects half a statement as though it were the whole one.
Example: "The brake fluid must be DOT 4..." → truncates to "The brake fluid mu..."
This is actively dangerous: an agent acting on truncated context may hallucinate
the rest, or worse, act on an incomplete instruction. The recall contract must
never inject partial truths as whole truths.

### Why summary-swap (B) over whole-item drop (C)?
A well-written summary preserves the *semantic gist* of the memory within the
token budget. Dropping entirely loses all signal. The summary is:
- Written at insert time by the extraction LLM (or provided explicitly)
- Pre-tokenized (`summary_tokens` recorded at write time)
- Guaranteed to be self-contained (not a fragment)

The writer pays the cost once (at insert); the reader pays nothing extra.

### Why whole-item drop as fallback?
If even the summary doesn't fit, the item is dropped whole. No partial injection,
no truncation. The packing algorithm simply moves to the next candidate.

---

## Packing Algorithm (D25)

### 1. Pinned First
Pinned memories are injected first, in rerank order. They are **not** bound by
tier shares — they claim budget before any tier share is calculated.

### 2. Tier Shares with Rollover
After pinned, tiers are served in declared order:
1. Working (40%)
2. Episodic (30%)
3. Semantic (20%)
4. Procedural (10%)

Each tier is offered `budget_tokens × share` tokens. Unused share rolls down to
the next tier (episodic gets working's leftovers, etc.). The ceiling
`Σ placed ≤ budget_tokens` is the invariant; shares are a preference.

### 3. Placement Decision per Item
For each candidate in rerank order (within its tier):

```
if full_text_tokens ≤ remaining_budget:
    place Full
else if summary_tokens ≤ remaining_budget:
    place Summary  (summary-swap)
else:
    place Dropped(TotalBudget or TierBudget)
```

- `TierBudget`: the tier's remaining share (plus rollover) was too small
- `TotalBudget`: the tier share had room but the global ceiling was hit
- `Unresolved`: canonical row missing (invariant violation, fails closed)

**Invariant:** `Σ placed tokens ≤ budget_tokens` holds after every placement.

---

## Why Not Round-Robin / Score-Order Packing?

Tier-order packing (not score-order) means a strong `semantic` memory can be
starved by a weaker `working` one. This is intentional: the split is a *policy
preference* about which tier's signal the agent should see first. Rollover
softens it whenever an earlier tier is quiet.

---

## Edge Cases

| Scenario | Behavior |
|----------|----------|
| Pinned item's full text exceeds budget | Dropped (pinned not exempt from ceiling) |
| Pinned summary fits but full text doesn't | Summary injected (summary-swap) |
| Summary doesn't fit either | Dropped (whole-item) |
| Summary tokens not recorded (null/≤0) | Treated as no summary → full or dropped |
| Tier share rounded up exceeds budget | Ceiling invariant enforced on every placement |

---

## Audit Trail
Every `recall()` writes `token_ledger` row with `tier_split_json` (serialized
`TierTokens`), `items_injected`, `items_dropped`. `recall_items` has `injected`
boolean and `components_json` per candidate.

---

## Alternatives Considered

| Option | Pros | Cons |
|--------|------|------|
| Truncate (A) | Never loses items | Injects partial truths; dangerous |
| Whole-item drop only (C) | Simple; never partial | Wastes budget when summary would fit; loses all signal from dropped items |
| Score-order packing | Maximizes score per token | Violates tier preference; no rollover semantics |

---

## Testing
- `tests/recall_budget.rs`: 5 tests covering ceiling exactness, whole-item drop, pinned placement, summary-swap, tier split + rollover.
- `tests/recall_audit.rs`: verifies `token_ledger` and `recall_items` rows match packing decisions.