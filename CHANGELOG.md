# Changelog

All notable changes to agos-memory are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/); versioning
follows [SemVer](https://semver.org/).

## [0.2.0] — 2026-09-20

The write path: facts go in, durably, with provenance.

### Added
- **Write path** (`agos_memory::memory`): `remember()` redacts, assigns trust,
  embeds, dedups, and persists a memory together with its `memory_versions`
  v1 row and its `vec_memories` row.
- **Sessions and turns**: `open_session`, `append_turn`, `close_session`,
  `close_idle_sessions`; idle timeout configurable via `[session]
  idle_minutes` (default 30).
- **Durable jobs + worker**: `enqueue` (idempotency keys), `claim_next`,
  `complete`, `fail` with exponential backoff, `jobs_dead` DLQ and
  `requeue_dead`; `Worker` dispatches job kinds to async handlers with
  graceful shutdown.
- **Extraction pipeline**: turn history → prompt → chat completion → JSON
  candidates, validated per item (tier/kind enums, text bounds, 0..1
  scores); malformed items are dropped, the batch survives.
- **Dedup**: cosine similarity over `vec_memories` within a tier; above
  `[memory] dedup_threshold` (default 0.92) the existing memory absorbs the
  candidate — `ref_count` bumped, `last_referenced_at` refreshed, `refines`
  link added, `dedup_cluster_id` set.
- **Redaction**: `sk-…`, `Bearer …`, PEM private-key blocks and
  `password=`/`api_key=`-style pairs are replaced with `[REDACTED]` before
  embedding and before insert; secrets therefore never reach the provider,
  `memories.text`, or a `memory_versions` row.
- **Provenance and trust**: `tool`, `web` and `import` sources produce
  `trust = 'untrusted'` memories; confidence below
  `[memory] pending_threshold` (default 0.4) stores `status = 'pending'`.
- **`openai_compat` providers** over the shared HTTP client: embeddings and
  chat against agos-proxy, each call appended to the `llm_calls` ledger.
- **CLI**: `remember --text <t> [--tier|--kind|--source-kind|--confidence]`
  and `session open|append|close|idle-close`.
- **Budget guard**: `[budget] max_tokens_per_session` refuses further
  extraction past the ceiling with `Error::BudgetExceeded` instead of
  silently overspending.
- Acceptance suites: `tests/write_path_restart.rs`, `tests/dedup.rs`,
  `tests/trust_provenance.rs`, `tests/redaction.rs`, `tests/jobs_dlq.rs`,
  `tests/providers.rs`.
- Docs: ADR-004 (shared HTTP client), ADR-005 (flock lock liveness),
  architecture write-path section, `make gate-v0.2.0`, `make smoke`.

### Fixed
- **sqlite-vec cosine conversion**: sqlite-vec's `distance_metric=cosine`
  returns `1 - cos`, but the dedup path decoded it as an L2 distance
  (`1 - d²/2`). The effective cutoff was cosine ≈0.6 instead of the agreed
  0.92, so unrelated facts were being merged. Now decoded correctly and
  covered by `tests/dedup.rs`.
- `Cargo.lock` is now committed (it was caught by the `*.lock` gitignore
  pattern, which also covered the runtime `<db>.lock` sidecar; the pattern is
  now `*.db.lock`).

### Changed
- `version = "0.2.0"`; new dependency `reqwest` with
  `default-features = false, features = ["json", "rustls-tls"]` — no OpenSSL
  or system TLS dependency is introduced.
- `Config` gains `[session]`, `[memory]` and `[budget]` sections.

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
