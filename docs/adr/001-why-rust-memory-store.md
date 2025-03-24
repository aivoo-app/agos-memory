# ADR-001: Why a Rust memory store backed by SQLite

Status: accepted, 2026-09-19

## Context

AGOS agents (Hermes, OpenClaw, future systems) need persistent memory that
survives restarts, supports citations, and runs on small self-hosted boxes
(one host OOM-killed a Hermes container at 908 MB RAM). agos-proxy already
standardized on Rust + rusqlite (bundled SQLite).

## Decision

Build agos-memory in Rust with one SQLite database per agent, WAL mode, and
the sqlite-vec extension for vector search.

## Rationale

- **SQLite** gives durability, backup tooling (`VACUUM INTO`), FTS5 for the
  keyword fallback path, and single-file operational simplicity (rsync a
  file and it is backed up).
- **sqlite-vec** keeps vectors in the same database with metadata columns
  that filter *inside* the KNN scan — our "hard filter before scoring"
  requirement (forgotten/deprecated rows can never be resurrected by
  similarity).
- **Rust + rusqlite** matches the sibling project's stack and the crate's
  reliability goals; the bundled SQLite build makes the binary portable.

## Alternatives considered

- **Postgres + pgvector**: correct choice at millions of memories or many
  concurrent writers. Deferred behind ADR (v0.6.0 escape hatch); SQLite is
  strictly simpler to self-host.
- **DuckDB / LanceDB / Qdrant**: separate engines would fragment the write
  path; the extraction pipeline needs transactional memory + jobs + audit in
  one store.
- **JSON/markdown files** (OpenClaw memory-core style): proven for capture,
  but weak for decay, citations, verified deletion, and hard filters.

## Consequences

- One writer per database is a hard constraint; enforced with a process lock
  and the writer-actor design (ADR-003).
- Vector recall is exhaustive scan; ~100k memories is the design target with
  a measured benchmark gate (R1).
