# ADR-010: pgvector escape hatch after the 100k recall miss

Date: 2026-09-24 · Status: accepted · Applies to: v0.6.0

## Context

The v0.3.0 claim was p95 < 150 ms at 10k; v0.6.0 added the harder p95 < 300 ms
claim at 100k. The first reproducible release measurements invalidate both on
the reference host:

- 10k: p50 698.57 ms, p95 928.09 ms, p99 1063.03 ms (100 samples);
- 100k: p50 6815.04 ms, p95 7472.74 ms, p99 7472.74 ms (20 samples).

Exact commands, hardware, seed times, and limitations are published in
[`docs/proof.md`](../proof.md). The 100k result is 24.91× the target, so this is
not a marginal miss that justifies moving the gate.

The 10k stage breakdown also matters:

- deterministic embedding: approximately 3.5 µs;
- keyword-only recall: approximately 368 ms;
- full hybrid recall: approximately 695 ms.

A vector-only migration is therefore insufficient. The prototype must address
both approximate vector search and keyword retrieval. Replacing sqlite-vec with
pgvector while retaining the current FTS leg would still miss the hybrid gate.

## Options

### 1. Keep sqlite-vec unchanged

This preserves the single-file deployment, `VACUUM INTO` backup, FTS5 triggers,
one-process lock, and the current `MemoryApi`. It is operationally simple, but
the measured linear-scan cost is incompatible with the published 10k/100k
claims. Keeping it unchanged while describing those targets as met is rejected.

### 2. Tune the SQLite index before changing engines

Evaluate sqlite-vec ANN/partition options and query shape first. This is the
lowest migration cost and may be sufficient if it can produce p95 < 300 ms at
100k without recall-quality loss. The current benchmark does not configure an
ANN partition, so this path remains plausible but unproven.

### 3. PostgreSQL + pgvector + PostgreSQL full-text search

pgvector HNSW can avoid a full vector scan, while PostgreSQL full-text search
can replace or accelerate the keyword leg. This adds a server, connection
pool, migrations, backup/restore operations, and a second consistency boundary.
It is justified as a bounded prototype because the measured miss is large; it
is not automatically justified as an immediate production cutover.

## Decision

1. **GO for a bounded prototype; NO-GO for immediate migration.** Build a
   separate, non-default prototype that stores the same memory/version/trust/
   provenance records in PostgreSQL, uses pgvector HNSW for the vector leg, and
   uses PostgreSQL full-text search for the keyword leg. Keep SQLite as the
   supported default through v0.6.0.
2. **Benchmark the complete hybrid path.** A vector-only p95 result cannot
   satisfy this ADR. The prototype must run the same hard filters, RRF,
   rerank, packing, and audit behavior behind the existing `MemoryApi` contract.
3. **Do not weaken correctness for speed.** The prototype must pass the current
   recall eval floors and zero-leak tests, preserve strict trust filtering, and
   make hard purge remove canonical, version, vector, FTS, recall-item, and pin
   records in one failure-closed transaction.
4. **Migration requires an operating model.** `VACUUM INTO` snapshots do not
   back up PostgreSQL. Before any cutover, the prototype must demonstrate
   point-in-time recovery, restore into a fresh cluster, row/vector/index
   verification, and an operator runbook. The existing single-writer actor and
   `<db>.lock` rule also need explicit replacement semantics.
5. **Re-evaluate SQLite tuning in parallel.** If indexed sqlite-vec reaches the
   target with equivalent quality and simpler operations, keep SQLite.

## Falsifiable triggers

- **Observed now:** sqlite-vec 100k p95 is 7472.74 ms, above 300 ms. This opens
  the prototype; it does not by itself approve migration.
- **Keep SQLite if** an indexed sqlite-vec configuration measures p95 < 300 ms
  at 100k on the reference workload, preserves the eval floors, and passes the
  leak test.
- **Make pgvector the migration candidate if** the complete PostgreSQL hybrid
  prototype measures p95 < 300 ms at 100k, has no hard-filter leak, preserves
  the current `MemoryApi` behavior, and passes restore drills.
- **Reopen the storage decision immediately if** production requires multiple
  concurrent server processes per logical agent, corpus growth is expected
  above one million memories, or the tuned SQLite path remains above 300 ms.
- **Do not migrate if** a vector-only prototype passes while keyword/hybrid p95
  still misses 300 ms; the result does not address the product claim.

## Consequences

- The v0.6.0 performance claims are published as **NOT MET**, not silently
  retuned.
- Operators retain the current SQLite deployment while the prototype is
  evaluated; no runtime dependency is added in this ADR.
- A future migration is a storage/operations project, not a one-line backend
  swap. FTS consistency, append-only audit guarantees, process locking, export,
  backup, and restore all need explicit designs.
- The prototype must remain outside the default configuration until ADR-010's
  migration criteria are all met.
