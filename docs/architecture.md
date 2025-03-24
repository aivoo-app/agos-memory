# Architecture

agos-memory is a self-hosted agent memory manager. One SQLite database per
agent; interfaces (MCP + HTTP) arrive in v0.5.0. This document describes the
foundation (v0.1.0) and the committed shape of what follows.

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
├── cli/             clap root + init/status/doctor
├── config.rs        layered config; fail-closed bind validation
├── error.rs         typed errors, actionable messages
├── storage/
│   ├── schema.rs    migrations (PRAGMA user_version), PRAGMA hardening
│   ├── vecext.rs    sqlite-vec registration + verification
│   ├── pool.rs      round-robin read connections
│   ├── writer.rs    single-writer actor (one OS thread owns the write conn)
│   └── store.rs     StoreHandle facade + process lock
├── embed/           Embedder trait: openai_compat | hash mock | none
├── llm/             ChatClient trait + deterministic mock
├── observe/         tracing init + llm_calls cost ledger
├── eval/            offline eval harness (precision/recall/MRR/leaks)
└── util/            clock (SystemClock/FakeClock), token counter, sha256
```

## Concurrency model

See [ADR-003](adr/003-single-writer-actor.md): one writer thread owns the
write connection; reads use a WAL read pool; `spawn_blocking` keeps blocking
SQLite off async workers; a sidecar lock file enforces one process per
database with stale-pid reclamation.

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

## Roadmap topics

- Write path (v0.2.0): extraction via `ChatClient`, durable jobs with DLQ,
  dedup by text hash + cosine, confidence-gated `pending` status.
- Recall (v0.3.0): embed -> KNN + BM25 -> hard filter -> rerank
  (`w1*sim + w2*importance + w3*decay`) -> tier-split packing -> fenced
  citations; no-hit returns explicit empties; everything audited.
- Forgetting (v0.4.0): soft deprecation by default; hard delete purges row +
  FTS + vector + versions, then `secure_delete` + `VACUUM` + tombstone.

[ADR-002]: adr/002-embedding-pluggable-and-pinned.md
