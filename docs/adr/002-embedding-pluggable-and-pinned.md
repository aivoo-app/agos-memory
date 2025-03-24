# ADR-002: Embeddings are pluggable and pinned per database

Status: accepted, 2026-09-19

## Context

Embedding models change; dimensions are baked into the `vec0` virtual table
at creation. Hosts are RAM-constrained, so a local ONNX model must not be a
default. AGOS already runs agos-proxy as an OpenAI-compatible gateway.

## Decision

1. `Embedder` trait with three providers:
   - `openai_compat` — default; pointed at agos-proxy.
   - `none` — degraded keyword-only recall (FTS5), used when no provider is
     configured or the session token ceiling is breached.
   - `local` (fastembed) — opt-in behind the `local-embed` cargo feature;
     never default.
2. The database pins `embed_model` + `embed_dim` in `meta` at first init.
   On open, a provider mismatch is refused with an actionable error and a
   pointer to the `reembed` job.
3. Embeddings are cached by content hash (`embeddings_cache`) so identical
   text is never re-embedded, and re-embedding after a model change is a
   bounded, resumable job.

## Consequences

- Switching models is a deliberate migration, not an accident.
- Tests and evals use the deterministic `HashEmbedder`; CI needs no network.
- The 908 MB Hermes host runs zero model weights by default.
