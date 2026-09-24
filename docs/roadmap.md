# Roadmap

Committed version plan. This file is the repo-visible source of truth.

| Version | Scope | Gate |
|---|---|---|
| v0.1.0 | Foundation & instrumentation: config, errors, store (WAL + migrations + sqlite-vec), single-writer actor + read pool, clock, token counter, llm_calls ledger, embedder/chat traits + mock providers, eval harness skeleton, CLI init/status/doctor, CI | make check green; migrations idempotent; vec0 KNN verified; fts5 verified |
| v0.1.1 | Gate integrity & debt: process-lock liveness (flock), CI, integration test suites, verified backup snapshots, dep hygiene | make ci green; stale lock reclaimed; snapshot verified row-for-row |
| v0.2.0 | Write path & tiers: sessions/turns, extractor pipeline, jobs + DLQ + idempotency, dedup/refcount, confidence/pending, provenance/trust, redaction, remember() | facts survive restart; duplicate bumps refcount; DLQ on failure |
| v0.3.0 | Recall: hybrid FTS5 BM25 + vec KNN, hard filter pre-scoring, rerank + decay, summary-swap, tier-split packing, whole-item drop, no-hit path, citations, explain | eval precision/recall/MRR reported; 10k-vector p95 < 150ms; zero hard-filter leaks |
| v0.4.0 | Consolidation & forgetting: session summaries, episodic compression, versioning/supersedes, procedural triples, forget soft/hard + cascade + secure_delete + tombstones, nightly maintain, replay audit | leak test passes on every path; cascade complete; nightly resumable |
| v0.5.0 | Interfaces: rmcp MCP server (stdio + Streamable HTTP), axum JSON API + bearer auth, agos-proxy clients, export/import, Hermes/OpenClaw docs, musl build, Docker | MCP stdio e2e test; integration docs verified |
| v0.6.0 | Proof & hardening: 100k load test <300ms, cost report, poisoning tests, backup/restore drill, eval regression gate, pgvector ADR, soak test | **shipped**; all six §10 evidence rows published; 10k/100k latency targets explicitly **NOT MET** |
| v1.0.0 | Release | every "Done Means" item evidenced |

Status: v0.3.0 shipped (recall path, eval gate, perf bench); v0.4.0 shipped
(consolidation & forgetting — summaries + ROUGE-L gate, versioning/rollback,
verified deletion with tombstones, TTL retention + reaper, consolidation job,
scheduler); v0.5.0 shipped (interfaces — MCP stdio + Streamable HTTP, axum
JSON API + bearer auth, export/import, musl static build, Docker); **v0.6.0
shipped** (proof & hardening — all six §10 claims evidenced in
[proof.md](proof.md), with reference-host 10k/100k latency targets explicitly
**NOT MET**).
