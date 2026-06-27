# Changelog

All notable changes to agos-memory are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/); versioning
follows [SemVer](https://semver.org/).

## [Unreleased]

### Added
- **v0.6.0 leak proof** — `tests/leak_paths.rs` verifies a hard-purged fact is
  unreachable through recall, fresh snapshots, exports, every discovered SQLite
  table/FTS shadow, and raw DB/WAL/SHM bytes. Pre-purge snapshots intentionally
  remain a documented restore risk.
- **100k release gate** — `make bench-100k` seeds 100,000 vectors in bounded
  transactions, reports setup separately, and fails if hybrid recall p95 is
  300 ms or higher. `make bench` now explicitly runs the existing 10k/150 ms
  gate.
- **Proof artifacts** — `docs/proof.md` and ADR-010 publish the measured
  10k/100k results and decide to prototype pgvector + PostgreSQL FTS without an
  immediate production migration.
- **Operator cost report** — `agos-memory cost [--session] [--since] [--json]`
  reports provider tokens, estimated USD, per-purpose latency/cost, and
  `token_ledger` totals from one shared query.
- **Schema v5 session attribution** — `llm_calls.session_id` is nullable;
  extraction costs are charged to the originating session, while pre-v5 and
  sessionless calls remain visible only in totals rather than being guessed.

### Changed
- The per-session provider-token ceiling now sums only that session's attributed
  ledger rows, refuses work at the ceiling, and treats `max_tokens_per_session=0`
  as unlimited. `docs/observability.md` documents attribution and price caveats.

### Known limits
- The reference 10k and 100k latency targets are **NOT MET** (p95 928.09 ms and
  7472.74 ms respectively). Thresholds were not retuned; ADR-010 defines the
  reopen/migration criteria.

## [0.5.0] — 2026-09-22

MCP server runtime: stdio + streamable HTTP, CLI `serve`, hermetic e2e.

### Breaking
- `recall::render_report` now takes the query text (`report, texts, query,
  min_score`) so the no-hit line quotes the actual query.

### Added
- **MCP tool surface** (`agos_memory::mcp`): six tools — `remember`, `recall`,
  `forget`, `summarize`, `explain`, `status` — exposed via rmcp's
  `#[tool_router]` macro. Structured content (JSON) + fenced text content on
  every tool response.
- **Server transports** (`agos_memory::server`):
  - `stdio.rs`: `serve --stdio` speaks newline-delimited JSON-RPC over
    stdin/stdout via `rmcp::transport::io::stdio()`.
  - `http.rs`: `serve --http` speaks the streamable-HTTP MCP protocol
    (SEP-2567) over an axum router with bearer-token auth (fail-closed:
    non-loopback origin without a token → 401).
  - `auth.rs`: shared auth middleware used by the HTTP transport.
- **CLI `serve` command**: `serve --stdio` (default: HTTP), `--bind`,
  `--token`. Validates the bind/token before binding (fail-closed).
- **CLI `export` / `import` commands**: portable, tombstone-aware JSONL
  migration. `export [--tier X] [--out file]` streams only `active`/
  `deprecated` rows of the store's agent (hard-purged ids never leave; piping
  to stdout via `--out -`); `import [--dry-run] file` (or `-` for stdin) is
  idempotent by `text_hash` (dupes bump `ref_count`), versioned on conflicting
  `public_id` (never a silent overwrite), refuses tombstoned ids, and replays
  trust/status/provenance byte-for-byte. Supports `export | import -`.
- **Static musl build (`make build-musl`)**: builds
  `x86_64-unknown-linux-musl` with a prerequisite guard (musl target +
  `musl-gcc`) and an `ldd` static-link check; runbook "Static (musl) build"
  documents the one-time setup.
- **Docker (`Dockerfile` + `docker-compose.yml`)**: multi-stage image —
  `rust:1.98-bookworm` builds the static musl binary, runtime `alpine:3.20`
  runs it with zero add-ons. `ENTRYPOINT ["agos-memory","serve"]`, volume at
  `/data`, `/healthz` healthcheck. Compose requires `AGOS_MEMORY_TOKEN`
  (off-loopback bind is fail-closed), wiring it to `[server] token`. NB:
  runtime is **alpine** rather than the originally-planned
  distroless/static-debian12 — that base ships no shell/tools (not even an
  executable BusyBox), so the in-container healthcheck would be impossible;
  Alpine is musl-native and ~36 MB.
- **E2E tests**: `tests/mcp_stdio.rs` (hermetic `provider = "none"`,
  handshake + remember→recall over stdin/stdout) and `tests/mcp_http.rs`
  (401 without token, 200 with, remember→recall over the wire).
- **Dependencies**: `axum 0.8`, `tower 0.5`, `tokio-util 0.7` (rt),
  `reqwest 0.12` (json + rustls-tls + blocking).
- **Milestone gate `make gate-v0.5.0`** (fmt + clippy `-D warnings` + tests +
  plan-guard + release build + smoke + **smoke-serve** + eval +
  eval-summarize) and the `smoke-serve` recipe: spawns `serve --stdio`, runs a
  real `tools/call` roundtrip over stdin, then starts HTTP `serve` on an
  ephemeral loopback port and asserts `/healthz` 200. Wired into CI.
- **`docker` CI job + `make docker-build` / `make docker-smoke`** — the only
  end-to-end verification of the musl build (0006) and the container (0007).
- **`docs/adr/009-interfaces-transports-auth.md`** — D38 (one router/bind/auth
  layer/shutdown across both surfaces), D39 (docs-only clients), the
  hand-written-OpenAPI coverage decision, and the alpine runtime deviation.
- **`docs/interfaces.md`** — the complete v0.5.0 interface reference
  (transports, MCP tools, JSON routes, auth, error taxonomy), reviewed against
  the implementation.

### Fixed
- Tracing logs go to stderr, so `serve --stdio` stdout carries JSON-RPC only.
- `make smoke-serve` wrote a *relative* `db_path` into its throwaway config, so
  the smoke opened `<repo-root>/s.db` instead of a temporary database (and a
  killed run could leave it locked). It now writes an absolute path, and guards
  stdio/HTTP failures with the captured output instead of failing with a bare
  `make: *** Error 4`; an `EXIT` trap always reaps the server and temp dir.
- `docs/examples/__pycache__/*.pyc` was accidentally tracked; untracked, and
  `__pycache__/` + `*.pyc` are now gitignored.
- **`.github/workflows/ci.yml` was not valid YAML**, so GitHub Actions rejected
  the whole workflow and *nothing* was being gated in CI. Two step names
  contained an unquoted `: ` (`Offline eval gate (hermetic: deterministic hash
  embedder)` and the summarize-quality gate), which YAML reads as a nested
  mapping. Both reworded; the file now parses and the new `docker` job is live.
- **Packaging had no automated proof.** Building the image is the only
  end-to-end check of the musl static build (0006) and the container (0007), so
  a push-only `docker` CI job now builds the image, starts it with the compose
  bind/token shape, waits for `/healthz` to answer 200, and asserts the auth
  matrix (401 without a token, 200 with). `make docker-build` / `make
  docker-smoke` give local parity.

## [0.4.0] — 2026-09-22

Consolidation & forgetting: summaries, versioning, verified deletion, TTL retention.

### Added
- **Summarization** (`agos_memory::memory::summarize`): on-demand (`summarize_by_id`,
  `summarize_tier`) and periodic (`run_summarization_job`) generation with per-tier
  `summarize_after_days` eligibility; summaries stored on the memory row
  (`summary_text`, `summary_tokens`) and consumed by recall packing's summary-swap (D25).
- **Summarization CLI (0048)**: `summarize --id/--tier/--all/--force`; the LLM is built
  from `[llm]` config (`chat_from_config`), falling back to `MockChat` when offline.
- **Quality gate (0049)**: `agos_memory::eval::rouge_l` (LCS F1, case/punctuation
  insensitive) + `SummarizeReport::quality_score`; hermetic 100-case fixture gate
  (`fixtures/summarize_cases.jsonl`, `make eval-summarize`) asserting mean ROUGE-L
  ≥ 0.85 (measured: 0.913). The gate proves the summarize + score pipeline offline;
  live-model quality runs are provider-backed, not the CI gate.
- **Memory versioning (0042)**: every update appends a `memory_versions` row with
  `diff_json`; `rollback_memory` creates a new head from an old version, chain intact;
  `forget rollback <pid> --to-version <n>` CLI.
- **Verified deletion (0054)**: FK-safe `hard_purge_memory` (links both directions,
  versions, recall items, pins, vector, FTS-triggered) in one transaction with
  fail-closed per-table row-count verification, real `VACUUM` + `wal_checkpoint`,
  and a `tombstones` row. Schema v4 adds database-enforced immutability triggers on
  `tombstones` and `forget_audit`.
- **TTL retention (0044/0047)**: per-tier `ttl_<tier>_days`, grace period, and the
  TTL reaper — expired active memories are soft-deprecated with `ttl_deprecate` audit
  rows; deprecated memories past grace are hard-purged with `ttl_purge` audit rows.
- **Consolidation job (0045)**: summarization pass + cosine dedup + orphan-version
  cleanup, idempotent, configurable via `[consolidate]`.
- **Jobs, worker, scheduler (0055)**: worker `extract` handler (payload
  `{"session": N}`); consolidation folded into the `maintain` kind via payload
  (D35 — no schema CHECK change); `session close`/`idle-close` enqueue extraction
  best-effort; foreground scheduler (`maintain --schedule`) honoring
  `[memory] reaper_hour` and `[consolidate] day/hour` with pure-function
  `scheduler_tick` unit tests.
- **Consolidation benchmarks (0049)**: `benches/consolidate_bench.rs` (Criterion) and
  `tests/consolidate_bench.rs` (integration rate gates): batch summarize ≥ 100 mem/s,
  TTL reaper ≥ 1000 memories/s (release-profile gated, `AGOS_BENCH_ASSERT=1` opt-in).

### Fixed
- **Consolidation dedup uses stored embeddings (0056)**: cosine over the
  `vec_memories` blob (LEFT JOIN) — the same vectors persist/recall score — falling
  back to the deterministic text proxy only when a memory has no stored vector.
- **agent_id on every insert path** (found by the 0052 eval harness): writes no
  longer fall back to the `'default'` schema value, which made them invisible to
  recall for any other agent.
- **Recall bench p95 assertion** is release-profile gated (or `AGOS_BENCH_ASSERT=1`)
  so debug `cargo test --all-targets` runs the bench without flaky perf assertions.
- Clippy warnings across tests, benches, and lib (unused imports, `flat_map`
  patterns, `!hits.is_empty()`).

### Changed
- `version = "0.4.0"`; Makefile gains `eval-summarize` and `bench-consolidate`;
  `gate-v0.4.0` runs the summarize-quality gate; CI runs both perf benches on push.
- Deterministic `hash` embedder provider added for offline runs (0052).

## [0.3.0] — 2026-09-21

The recall path: hybrid retrieval, hard filters, rerank, packing, audit, citations.

### Added
- **Hybrid recall** (`agos_memory::recall`): vector KNN (sqlite-vec) + keyword BM25 (FTS5), fused by RRF (K=60), hard-filtered inside both legs and after fusion (zero-leak guarantee).
- **Rerank (D23/D24)**: blended score `w1·sim_norm + w2·importance_eff + w3·decay`; default weights 0.60/0.25/0.15; per-tier half-lives (working=6h, episodic=21d, semantic/procedural=∞).
- **Token packing (D25)**: 1500-token ceiling; tier split 40/30/20/10 with rollover; pinned first; whole-item drop or summary-swap, never truncation.
- **No-hit semantics (D26)**: `min_score` threshold (default 0.35); below it → `no_hit=true`, empty injection, never a weak hit; caller decides.
- **Trust policy (D29)**: `Strict` (default: trusted+system) vs `Fenced` (opt-in untrusted, rendered fenced as data).
- **Degraded mode (D4)**: embedding unavailable → BM25-only, `degraded=true`, never a panic.
- **Explain & citations (0035)**: `MemoryExplain` (provenance, version chain, links, ref_count, recall injections), `why()` rank boosters, fenced `<memory>` wire format with `trust` attribute.
- **Audit trail (0036)**: `recalls`, `recall_items`, `token_ledger` rows per call; `components_json` and `tier_split_json` round-trip through serde.
- **CLI commands (0037)**: `recall` (text, k, budget, trust filters, --json/--explain), `explain <id>`, `eval` (JSONL cases, precision/recall/MRR gates, deterministic HashEmbedder).
- **Perf benchmark (0038)**: `benches/recall_bench.rs` with Criterion — `recall_full`, `embed_only`, `vec_scan_degraded`, `recall_p95` (p95 < 150ms assertion); uses HashEmbedder for hermetic offline benchmark.
- **Integration suites (0039)**: 8 test suites — `recall_hybrid`, `recall_filters`, `recall_budget`, `recall_explain`, `recall_nohit`, `recall_degraded`, `recall_audit`, `eval_gate`.
- **Makefile**: `make eval`, `make bench`, `make gate-v0.3.0` (fmt + clippy -D + tests + plan-guard + build + smoke + eval + bench).
- **Docs**: `docs/recall.md` (full math spec), ADR-006 (hybrid retrieval), ADR-007 (token packing).

### Fixed
- **Hard filter zero-leak**: proved SQL predicate and Rust mirror agree row-for-row; `tests/recall_filters.rs` validates zero leaks on both retrieval paths.
- **No-hit rendering**: exact D26 phrasing `No useful memories for "<query>"` with min_score and candidates.
- **Degraded mode**: never panics; provider=none → BM25-only, `degraded=true`.
- **Packing**: ceiling exact, whole-item drop (no truncation), tier split respected, rollover works, summary-swap verified.

### Changed
- `version = "0.3.0"`; `Cargo.toml` bumped.
- `Makefile`: added `eval`, `bench`, `gate-v0.3.0` targets.
- `docs/architecture.md`: updated with recall path section.
- `Cargo.toml`: added `criterion` dev-dependency, `[[bench]]` target for `recall_bench`.

### Removed
- (none — all v0.2.0 features preserved)

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
