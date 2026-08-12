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

## Token-per-answer flatness

Command:

```sh
cargo test --test token_flatness -- --nocapture --test-threads=1
```

Result on 2026-09-24: **PASS**. With a 1,500-token budget and equal ~200-token
semantic items, recall injected 1,407 tokens at each history/candidate size:

| Corpus / candidates | Answer tokens | `token_ledger` tokens | Range ratio |
|---:|---:|---:|---:|
| 32 | 1,407 | 1,407 | — |
| 128 | 1,407 | 1,407 | — |
| 512 | 1,407 | 1,407 | **0.0000 (0.00%)** |

Threshold: `< 5%`. Every case returned seven non-empty items, and report
accounting matched the independent `token_ledger` row. A mutation that added one
budget token per candidate escaped the 1,500 ceiling at corpus 128 (1,608 tokens)
and was detected; production packing was restored unchanged.

## Aged-fact recall

Command:

```sh
cargo test --test aged_recall -- --nocapture --test-threads=1
```

Result on 2026-09-24: **PASS**. The real write path created the facts;
`recall_with_clock` and `FakeClock` controlled expiry/decay evaluation.

| Age | Old score | Fresh score | Old decay | Reachability |
|---:|---:|---:|---:|---|
| 90 days | 0.727852 | 0.870161 | 0.051271 | old returned second |
| 180 days | 0.720556 | 0.870161 | 0.002629 | old returned second |
| 365 days | 0.720162 | 0.870161 | 0.000006 | old returned second |

The 180-day fact remained above the default `min_score=0.35`; age reduced its
rank without making it unreachable. A pinned fixture scored `0.875000` at both
day 0 and day 365, with decay `1.0`, proving pinned rows ignore age.

## Backup/restore drill

Command:

```sh
make restore-drill
# equivalently:
cargo test --test restore_drill -- --nocapture --test-threads=1
```

Result on 2026-09-24: **PASS**. The drill created active semantic and episodic
memories, a two-version history, a soft-deleted memory, and a hard-purged
memory with tombstone/audit evidence. It took a verified `VACUUM INTO`
snapshot, refused a concurrent second `StoreHandle::open`, released the live
handle, replaced the database file, removed stale WAL/SHM sidecars, left the
`.db.lock` sidecar in place, and reopened the restored file through the normal
product path. Schema version, embedding dimension, integrity, memory/vector/FTS
counts, version history, status counts, tombstones, and forget-audit rows all
matched the pre-snapshot live database. Product recall returned the active
semantic and opted-in episodic facts, while the soft-deleted and hard-purged
facts remained absent.

The runbook now requires an offline replacement, atomic snapshot install,
verification with `doctor`/`status`, and a rollback path. It also records the
important pre-purge snapshot caveat: restoring a pre-purge backup resurrects
content deleted after that snapshot.

## Eval regression baseline

The offline eval corpus is 20 cases and uses the deterministic hash embedder.
The committed baseline was generated by the harness on 2026-09-24 from git
commit `728ee3a` with the default one-percentage-point tolerance:

| Metric | Baseline | Absolute floor | Result |
|---|---:|---:|---|
| Precision | 1.000 | >= 0.90 | PASS |
| Recall | 1.000 | >= 0.95 | PASS |
| MRR | 1.000 | >= 0.80 | PASS |
| Forbidden leaks | 0 | 0 | PASS |
| Cases | 20 | >= 20 | PASS |

Command:

```sh
make eval
```

`make eval` now checks the absolute floors first, then compares the measured
quality metrics with `fixtures/eval_baseline.json`. A drop greater than `0.01`
in precision, recall, or MRR fails with both `baseline:` and `measured:`
printed; any new forbidden leak fails exactly. The fixture test runs the
corpus twice and compares complete metrics for determinism. A query
perturbation test confirms the unchanged baseline rejects a real scoring
regression.

Baseline updates are explicit and reviewable:

```sh
AGOS_EVAL_UPDATE_BASELINE=1 make eval
```

The normal run never writes the artifact. The update path writes only after the
absolute floors pass and records the release, source commit, corpus size,
tolerance, and measured metrics. See [eval.md](eval.md) for the operator
workflow.

## Sustained mixed-traffic soak

Command:

```sh
make soak
# short diagnostic:
AGOS_SOAK_SECS=2 make soak
```

The release test uses a temporary database and the real `StoreHandle`, write
actor, read pool, recall path, durable jobs, and worker. Its lanes exercise:

- persisted remembers with varied tiers/kinds;
- varied recalls;
- session open/append/close and extraction jobs;
- soft forget and hard purge; and
- TTL-only and full maintenance (`ttl`, summarize, dedup, and orphan cleanup).

The session and maintenance producers are paced to a rate the intentionally
single-at-a-time worker can drain; this prevents an artificial queue backlog
from being mistaken for a deadlock. The session lane produces at most five
extractions per second and maintenance at most one job per second. The first
60-second attempt also found and fixed a real dedup defect: maintenance writes
textual `dedup-<hash>` cluster IDs, while the persistence path decoded the mixed
SQLite value as INTEGER.

It samples RSS and the WAL file while running, checkpoints the WAL, drains all
jobs, checks writer health, and reconciles every generated memory id and job
count.

The default 60-second release soak on 2026-09-24 produced:

| Measurement | Result |
|---|---:|
| Operations | 5,219 |
| Throughput | 86.98 ops/sec |
| Remember calls | 3,195 |
| Retained direct rows | 2,749 |
| Hard purges | 188 |
| Sessions / turns | 239 / 239 |
| Jobs drained | 298 / 298 |
| Jobs dead | 0 |
| RSS start / peak / end | 8,516 / 120,948 / 120,948 KiB |
| WAL peak / end | 4,124,152 / 4,124,152 bytes |
| Database start / end | 4,096 / 23,474,176 bytes |

The short `AGOS_SOAK_SECS=2` diagnostic run also passed; its 2-second sample was
305 operations, 152.50 ops/sec, 166 remember calls, 156 unique retained rows, 8 hard purges, 9
sessions/turns, 11/11 jobs drained, 80,108 KiB peak RSS, 1,792,232-byte peak
WAL, and a 6,901,760-byte final database. Both runs stayed below the default RSS,
WAL, and database growth ceilings. The default RSS growth ceiling is 128 MiB,
the default WAL ceiling is 32 MiB, and the default database growth ceiling is
64 MiB. Non-Linux hosts print an RSS skip notice because `/proc/self/statm` is
unavailable. The mutation hook `AGOS_SOAK_RETAIN_BYTES_PER_OP` retains buffers
per writer operation; the tight-bound command
`AGOS_SOAK_SECS=1 AGOS_SOAK_RETAIN_BYTES_PER_OP=1048576
AGOS_SOAK_MAX_RSS_GROWTH_KIB=32768 make soak` failed as expected on the RSS
assertion. See the [operations runbook](operations/runbook.md) for release
usage.



The 100k miss activates the escape-hatch condition. ADR-010 decides **go for
a bounded pgvector/PostgreSQL-FTS prototype, no-go for an immediate production
migration**. A migration still requires the same recall contracts, a p95 below
300 ms at 100k, zero hard-filter leaks, and verified backup/restore behavior.
