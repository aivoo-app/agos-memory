# ADR-008: Verified deletion & consolidation

Date: 2026-09-22 · Status: accepted · Applies to: v0.4.0

## Context

v0.4.0 adds the "forgetting" side of memory management. Three forces shaped the
design: users must be able to delete a memory **provably** (row, vector, FTS,
versions, audit residue); retention must run automatically without an operator;
and the durable jobs table (`jobs.kind` CHECK) cannot be altered without a
schema-v5 table rebuild (`jobs` has `jobs_dead` FK + `idempotency_key UNIQUE`).

## Decisions

1. **Two-phase deletion (D12, implemented)** — soft deprecate is the default
   (`forget --id --soft`); hard delete is explicit (`forget --id --hard`) and
   runs `hard_purge_memory`: delete `memory_links` (both directions),
   `memory_versions`, `recall_items`, `pins`, `vec_memories`, then the
   canonical `memories` row (FTS synced by trigger), all inside one writer
   transaction. Per-table `COUNT(*)` after the deletes must be zero — any
   survivor aborts the purge with the table named (fail-closed). After commit:
   `VACUUM` + `wal_checkpoint`, measured (`vacuum_duration_ms`,
   `rowcount_before`), and a `tombstones` row records what vanished.

2. **Append-only ledgers are database-enforced** — schema v4 adds
   `BEFORE UPDATE/DELETE → RAISE(ABORT)` triggers on `tombstones` and
   `forget_audit`. Immutability is a property of the database, not a convention
   in `store.rs`.

3. **Automated vs manual retention is distinguishable** — TTL actions write
   `forget_audit` rows with `action = 'ttl_deprecate'` / `'ttl_purge'` and
   requester `ttl-reaper`; manual actions keep `soft_deprecate` / `hard_purge`.
   Post-hoc, an operator can tell a user's `forget` from the nightly reaper.

4. **Consolidation folds into the `maintain` job kind (D35)** — adding a
   `consolidate` kind would require the v5 rebuild; instead `maintain` carries a
   JSON payload `{"action": "ttl" | "consolidate" | "summarize" | "all"}`. One
   kind, no CHECK migration, and the scheduler enqueues `maintain` jobs at
   `[memory] reaper_hour` / `[consolidate] day/hour` with idempotency keys so
   re-ticking is a no-op.

5. **Consolidation dedup scores stored embeddings** — cosine over the
   `vec_memories` blob (LEFT JOIN on `rowid`), i.e. the exact vectors the write
   path and recall score. Memories without a stored vector (degraded writes)
   fall back to the deterministic `hash_to_unit_vector` text proxy so verbatim
   duplicates still merge offline. Threshold identical to the write path
   (`[memory] dedup_threshold`, D20).

6. **Summaries live on the memory row, originals in `memory_versions`** —
   `summary_text`/`summary_tokens` are mutable (on-demand `--force`
   overwrites); the version chain stays immutable. Recall packing's
   summary-swap (D25) reads exactly these columns.

## Consequences

- Hard deletion is verifiable by replay: tombstone + zero row counts + VACUUM
  timing. `forget --list-tombstones` and `forget --list-audit` are the
  operator surfaces.
- The reaper is bounded by the grace period: nothing auto-purges before
  `ttl_grace_days` (default 30) after deprecation.
- A future schema migration must rebuild the `jobs` table to add kinds; until
  then, every new background operation is a `maintain` payload action.
