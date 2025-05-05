# Changelog

All notable changes to agos-memory are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/); versioning
follows [SemVer](https://semver.org/).

## [0.1.1] — 2026-09-20

### Fixed
- **Process-lock liveness**: the one-process-per-database lock is now an
  exclusive non-blocking `flock` on a `<db>.lock` sidecar. Previously the
  liveness check always reported "alive", so a killed process bricked the
  database until the lock file was deleted by hand. `DbLocked` errors now
  name the real holding pid (recorded in the sidecar) instead of a constant 0.

### Added
- `agos-memory backup --out <file>`: verified `VACUUM INTO` snapshots —
  integrity check plus core-table row-count equality against the live
  database; refuses to overwrite an existing target.
- Integration test suites: `tests/migrations.rs`, `tests/store_restart.rs`,
  `tests/sqlite_vec_knn.rs`, `tests/plan_guard.rs`, `tests/cli_smoke.rs`,
  `tests/backup.rs`.
- CI: `make plan-guard` (fails if anything under `plan/` besides its own
  `.gitignore` is tracked — the single file that keeps local planning notes
  untracked), `make gate-v0.1.1`, and a GitHub Actions pipeline
  (fmt, clippy `-D warnings`, offline tests, plan-guard, release build).
- `docs/roadmap.md` (the README linked to it before it existed).

### Changed
- Removed unused dependencies (`anyhow`, `axum`, `futures`); tokio features
  trimmed to `rt-multi-thread` + `macros` (lower footprint on RAM-constrained
  hosts per decision D4). `axum` returns deliberately with the v0.5.0 server.

## [0.1.0] — 2026-09-19

### Added
- Foundation & instrumentation release:
  - Layered configuration (defaults <- TOML <- env <- flags) with fail-closed
    non-loopback bind validation and redacted `Debug` output.
  - Typed error taxonomy with actionable messages and stable exit codes.
  - SQLite storage: schema v1 (full table set: sessions, turns, memories,
    versions, links, embeddings cache, jobs + dead-letter queue, LLM call
    ledger, recall audit, forget audit, tombstones), `PRAGMA user_version`
    migrations, WAL + secure_delete hardening.
  - sqlite-vec registration and `vec0` table with metadata-column hard
    filters (tier/status/trust) verified by KNN tests.
  - Single-writer actor + round-robin read pool; blocking SQLite work never
    runs on async workers; one-process-per-database lock with stale-pid
    reclamation.
  - Embedding abstraction with deterministic hash mock and degraded
    keyword-only mode; chat client trait with deterministic mock.
  - LLM call cost ledger with token estimates.
  - Offline eval harness (precision / recall / MRR / leak count).
  - CLI: `init` (config scaffold + database), `status`, `doctor`.
  - Makefile, GitHub Actions CI (fmt, clippy, tests, plan guard).
  - Docs: README, CONTRIBUTING, SECURITY, ADR-001..003.
