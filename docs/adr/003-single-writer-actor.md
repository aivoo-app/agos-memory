# ADR-003: Single-writer actor + read pool

Status: accepted, 2026-09-19

## Context

agos-proxy's local plan recorded a P0 defect (Issue 3): blocking SQLite
writes executed on async worker threads behind a single `Mutex<Connection>`,
serializing unrelated requests and stalling the tokio runtime under load.
agos-memory shares the rusqlite stack and inherits that failure mode.

## Decision

- **One write connection**, owned by a dedicated OS thread (the *writer
  actor*). Callers submit closures over an mpsc channel and receive results
  via a bounded reply channel.
- **A read pool** of N read-only connections, handed out round-robin. WAL
  mode allows readers to proceed concurrently with the writer.
- Async callers wrap reads in `spawn_blocking`; nothing blocking runs on a
  tokio worker thread.
- One process per database, enforced by a sidecar lock file with stale-pid
  reclamation, producing a clear error instead of `SQLITE_BUSY`.

## Consequences

- No lock contention between reads and writes beyond SQLite's own WAL
  bookkeeping; no async worker can be blocked by a slow write.
- The soak test (v0.6.0) asserts latency does not pile up under concurrent
  write+read load — a regression test for this exact failure class.
