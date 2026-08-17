# Architecture

agos-memory is a self-hosted agent memory manager. One SQLite database per
agent, reached through MCP (stdio + Streamable HTTP) or the JSON API — shipped
in v0.5.0. This document describes the foundation (v0.1.0), the committed shape
of what follows, and how the interface layer (v0.5.0) sits on top.

## Milestones

| Version | Scope |
|---------|-------|
| v0.1.0  | Storage, migrations, sqlite-vec, single-writer store, eval harness, CLI |
| v0.2.0  | Write path: sessions/turns, extraction pipeline, jobs + DLQ, dedup, trust |
| v0.3.0  | Recall path: hybrid FTS5 + vector retrieval, hard filters, rerank, packing |
| v0.4.0  | Consolidation & forgetting: summaries, versioning, verified deletion |
| v0.5.0  | Interfaces: MCP (stdio + Streamable HTTP), JSON API, integrations |
| v0.6.0  | Proof: load tests, cost report, leak tests, backups |

## Module map

```
src/
├── main.rs          binary entry: CLI parse, tracing, exit codes
├── lib.rs           library root
├── cli/             clap root + init/status/doctor/remember/backup/serve/export
├── config.rs        layered config; fail-closed bind validation
├── error.rs         typed errors, actionable messages
├── http.rs          shared HTTP client for providers (ADR-004)
├── storage/
│   ├── schema.rs    migrations (PRAGMA user_version), PRAGMA hardening
│   ├── vecext.rs    sqlite-vec registration + verification
│   ├── pool.rs      round-robin read connections
│   ├── writer.rs    single-writer actor (panic-safe, timeouts, health flag)
│   └── store.rs     StoreHandle facade, flock process lock, snapshot/backup
├── memory/          write path
│   ├── sessions.rs  sessions + turns, idle close
│   ├── jobs.rs      durable queue: retry/backoff, DLQ, idempotency keys
│   ├── worker.rs    polling worker dispatching jobs to handlers
│   ├── extract.rs   prompt -> chat JSON -> validated candidates
│   ├── persist.rs   embed, cosine dedup, insert memory + version + vector
│   ├── redact.rs    secret redaction before embed and before insert
│   └── mod.rs       `remember()`: the write-path entry point
├── api/             transport-neutral `MemoryApi`: the six operations both
│                    MCP and the JSON API call (0002)
├── recall/          recall path: hybrid FTS5 + vec KNN, filter, rerank, packing
├── mcp/             MCP tool surface (rmcp 3.4): six tools → `MemoryApi`
├── server/          transports: stdio + Streamable HTTP, JSON API, bearer auth
├── embed/           Embedder trait: openai_compat | hash mock | none
├── llm/             ChatClient trait: openai_compat | deterministic mock
├── observe/         tracing init + llm_calls cost ledger
├── eval/            offline eval harness (precision/recall/MRR/leaks)
└── util/            clock (SystemClock/FakeClock), token counter, sha256
```

CLI commands: `init`, `status`, `doctor`, `backup --out <file>`,
`remember --text <t>`, `session open|append|close|idle-close`,
`serve [--stdio] [--bind] [--token]` (v0.5.0), `export|import` (JSONL
migration), `cost [--session <id>] [--since <dur>] [--json]` (v0.6.0),
`forget` (v0.4.0). Full interface reference:
[interfaces.md](interfaces.md).

## Concurrency model

See [ADR-003](adr/003-single-writer-actor.md): one writer thread owns the
write connection (with send/receive timeouts and panic isolation); reads use
a WAL read pool; `spawn_blocking` keeps blocking SQLite off async workers; a
sidecar `<db>.lock` held with a non-blocking `flock` enforces one process per
database — the kernel releases the lock if the holder dies, so stale locks
self-heal on the next open.

## Storage

- Schema versioning via `PRAGMA user_version`; newer databases are refused
  with an upgrade message.
- FTS5 external-content table over `memories.text` with insert/update/delete
  triggers (keyword fallback path, works in degraded mode).
- `vec_memories` vec0 table with cosine distance and metadata columns
  (`tier`, `status`, `trust`, `kind`, `pinned`) so hard filters apply inside
  the KNN scan.
- Embedding dim is pinned in `meta` at first init ([ADR-002]).

## Data model (v1 schema)

`agents`, `meta`, `sessions`, `turns`, `memories` (tier/kind/status/trust/
provenance/importance/confidence/versioning fields), `memory_versions`,
`memory_links` (supersedes/contradicts/...), `embeddings_cache`,
`jobs` + `jobs_dead` (durable extraction queue with idempotency keys),
`llm_calls` (cost ledger), `token_ledger` (per-answer budget), `recalls` +
`recall_items` (recall audit), `pins`, `forget_audit`, `tombstones`.

## Write path (v0.2.0)

`remember()` is the single write-path entry point (CLI, and since v0.5.0 also
via `MemoryApi` → MCP/JSON). One call performs, atomically per database
transaction where it matters:

1. **Redact** — `sk-…`, `Bearer …`, PEM private-key blocks and
   `password=`/`api_key=`-style pairs become `[REDACTED]` *before* the text is
   embedded or stored, so a secret never reaches the provider, the vector
   table, or `memories.text`.
2. **Trust** — provenance decides trust: `tool`, `web`, `import`, and `file`
   sources produce `trust = 'untrusted'` memories; only `user` and `agent` are
   trusted. The default recall policy never injects them ([ADR-001] lineage,
   D13).
3. **Status** — candidates below
   `[memory] pending_threshold` (default 0.4) are stored as
   `status = 'pending'` rather than `active`.
4. **Embed** — via the configured provider (ADR-004); with
   `provider = 'none'` the store degrades to keyword-only and no vector row
   is written.
5. **Dedup** — a KNN lookup over `vec_memories` in the same tier compares
   cosine similarity. Above `[memory] dedup_threshold` (default 0.92) the
   existing row wins: `ref_count` is bumped, `last_referenced_at` refreshed,
   a `refines` link is added, and `dedup_cluster_id` is set. Trust is merged
   conservatively: an untrusted collision downgrades a trusted survivor, while
   a trusted collision cannot upgrade an untrusted survivor. Otherwise a new
   row is inserted together with its `memory_versions` v1 row and its vector.
6. **Record** — extraction is a durable job (`extract`) with an idempotency
   key, `max_attempts`, exponential backoff, and a `jobs_dead` DLQ the
   operator can requeue. Every provider call appends to the `llm_calls`
   cost ledger; a per-session ceiling (`[budget] max_tokens_per_session`)
   refuses further extraction rather than silently overspending.

## Roadmap topics

- Write path (v0.2.0, **shipped**): see above — sessions/turns, extraction via
  `ChatClient`, durable jobs with DLQ, redaction, cosine dedup, confidence-gated
  `pending` status.
- Recall (v0.3.0, **shipped**): embed -> KNN + BM25 -> hard filter -> rerank
  (`w1*sim + w2*importance + w3*decay`) -> tier-split packing -> fenced
  citations; no-hit returns explicit empties; everything audited. Full math
  spec in [recall.md](recall.md).
- Consolidation & forgetting (v0.4.0, **shipped**): summaries with a ROUGE-L
  quality gate, version rollback, verified deletion (fail-closed purge +
  `VACUUM` + tombstones, append-only ledgers), per-tier TTL with grace +
  reaper, and a consolidation job (summarize pass + stored-vector dedup +
  orphan cleanup) driven by the `maintain` job kind and a foreground
  scheduler. Full spec in [forget.md](forget.md); rationale in
  [ADR-008](adr/008-verified-deletion-and-consolidation.md).
- Interfaces (v0.5.0, **shipped**): one `serve` process exposes MCP
  (Streamable HTTP at `/mcp`, or `--stdio`) and the JSON API under `/api/v1`
  on a single axum 0.8 router, bind, bearer-auth layer and shutdown path
  (D38). Both transports are thin shells over `src/api`'s `MemoryApi`, so the
  business rules (budget guard, `provider = 'none'` degradation, trust
  derivation) live in exactly one place. Auth is fail-closed: a non-loopback
  bind without a token (or with one shorter than 16 chars) refuses to start.
  Packaging: `make build-musl` static binary + a Docker image. Full reference
  in [interfaces.md](interfaces.md); rationale in
  [ADR-009](adr/009-interfaces-transports-auth.md).
- Performance proof (v0.6.0, **in flight**): the release 10k and 100k hybrid
  gates are measured and **NOT MET** on the reference host; the 100k miss opens
  a bounded pgvector/PostgreSQL-FTS prototype but not an immediate migration.
  Reproducible numbers are in [proof.md](proof.md); decision and falsifiable
  migration triggers are in [ADR-010](adr/010-pgvector-escape-hatch.md).

[ADR-001]: adr/001-why-rust-memory-store.md
[ADR-002]: adr/002-embedding-pluggable-and-pinned.md
[ADR-004]: adr/004-http-client.md
[ADR-005]: adr/005-lock-liveness-flock.md
