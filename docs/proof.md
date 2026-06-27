# v0.6.0 proof log

This file is the repo-visible evidence ledger for the v0.6.0 “Proof &
hardening” milestone. A row is complete only when its command, environment,
date, and measured value are recorded here. **NOT MET** is a valid result: the
threshold is never moved to make a run pass.

## Measurement environment

- Date: **2026-09-24**
- OS/kernel: Linux `7.2.6-zen2-1-zen`, `x86_64`
- CPU: Intel Core i5-6300U @ 2.40 GHz, 2 cores / 4 threads
- RAM: 8,009,556 kB (approximately 7.64 GiB)
- Toolchain: `rustc 1.98.1`, `cargo 1.98.1`
- Profile: release, `opt-level=3`, fat LTO, symbols stripped
- Benchmark shape: 1,536-dimensional vectors, batched seed size 1,000,
  deterministic `HashEmbedder`; setup time is reported separately from query
  percentiles.
- Important limitation: every benchmark vector is identical. This is a scan
  and hybrid-pipeline latency test, not an ANN-recall-quality test.

## Current evidence

| Claim | Command | Samples | Seed | p50 | p95 | p99 | Threshold | Result |
|---|---|---:|---:|---:|---:|---:|---:|---|
| Recall @10k | `make bench` | 100 | 15.37 s | 698.57 ms | 928.09 ms | 1063.03 ms | p95 < 150 ms | **NOT MET** (6.19×) |
| Recall @100k | `make bench-100k` | 20 | 150.90 s | 6815.04 ms | 7472.74 ms | 7472.74 ms | p95 < 300 ms | **NOT MET** (24.91×) |

`make bench-100k` deliberately uses 20 explicit samples (minimum accepted by
the benchmark) so a multi-second scan can be reproduced on a four-thread host.
With 20 observations, p95 and p99 select the same order statistic; the full
sample count is printed to prevent over-interpreting that value. The 10k run
uses the default 100 samples and publishes the Criterion stage breakdown:

- full hybrid recall: 680.41–710.50 ms Criterion interval
- keyword-only recall: 360.57–378.26 ms Criterion interval
- hash embedding: 3.4537–3.5497 µs Criterion interval

The embedding provider is not the bottleneck. Both the vector/hybrid path and
FTS5 path exceed the 10k target on this hardware, so changing only the vector
backend would not establish the 300 ms hybrid claim.

## Forgotten-item leak proof

Command:

```sh
cargo test --test leak_paths -- --nocapture --test-threads=1
```

Result on 2026-09-24: **PASS**.

The test writes one bait through `MemoryApi`, proves it is present in recall,
export, and a pre-purge snapshot, hard-purges it, and then verifies:

- recall no longer returns its public id;
- every dynamically discovered SQLite table/column is clean, including FTS5
  shadow tables and an FTS5 `MATCH` probe;
- a fresh snapshot is SQL-clean and raw-byte-clean;
- a fresh JSONL export contains neither bait text nor public id;
- the checkpointed live DB, WAL, and SHM contain no bait bytes.

A snapshot created before the purge intentionally retains the bait. Snapshots
cannot retroactively forget; see [forget.md](forget.md) and the
[operations runbook](operations/runbook.md#retention--forgetting-v040).

## Performance decision

The 100k miss activates the escape-hatch condition. ADR-010 decides **go for
a bounded pgvector/PostgreSQL-FTS prototype, no-go for an immediate production
migration**. A migration still requires the same recall contracts, a p95 below
300 ms at 100k, zero hard-filter leaks, and verified backup/restore behavior.
