# ADR-004: One shared HTTP client (reqwest, rustls, no OpenSSL)

Status: accepted, 2026-09-20

## Context

The write path needs two provider calls against agos-proxy's OpenAI-compatible
surface: `/v1/embeddings` and `/v1/chat/completions`. Choices ranged from a
hand-rolled `hyper` client (already in the tree transitively via axum, but
that dependency was removed in v0.1.1) to `reqwest`. The deployment target is
a small self-hosted box where an OpenSSL build dependency is a real
installation hazard, and every test must run offline.

## Decision

- `reqwest` with `default-features = false, features = ["json", "rustls-tls"]`.
  No `native-tls`, no `openssl-sys`, no system TLS libraries.
- All provider traffic goes through one module, `src/http.rs`:
  `HttpConfig { base_url, api_key, timeout }` plus
  `post_json<Req, Resp>(...)`, which sets the bearer token, applies the
  configured timeout, and maps every failure onto a typed error
  (`Error::Embedder` for embeddings, `Error::Llm` for chat) that names the URL.
- The base URL is trailing-slash-normalized once, at construction.
- **No retries in the client.** Retry/backoff policy belongs to the durable
  jobs layer (issue 0026), which owns `max_attempts`, backoff, and the DLQ;
  a client-side retry would hide failures from that policy and double-count
  ledger entries.

## Consequences

- Offline unit tests build a single-shot TCP stub on `127.0.0.1:0`
  (`tests/providers.rs`), asserting the auth header and the request path
  without any network access.
- Timeouts are per client instance, so a slow embedding endpoint cannot stall
  a chat extraction, and neither can hang a caller indefinitely.
- Errors are actionable: a dimension mismatch tells the operator the expected
  dim; a transport failure names the URL that failed.
- Swapping providers later (another OpenAI-compatible gateway) needs no new
  dependency — only a different `base_url`/`model`.
