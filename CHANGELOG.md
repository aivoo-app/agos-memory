# Changelog

All notable changes to agos-memory are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/); versioning
follows [SemVer](https://semver.org/).

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
