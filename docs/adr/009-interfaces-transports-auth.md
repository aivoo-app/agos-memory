# ADR-009: Interfaces — transports, auth, tools, routes

Date: 2026-09-23 · Status: accepted · Applies to: v0.5.0

## Context

v0.5.0 opens the library to agents and operators over MCP and plain HTTP.
Three forces shaped the design: agents need a standard protocol to
remember/recall (MCP is the Model Context Protocol standard); operators need a
simple JSON/HTTP surface that needs no client SDK; and the existing
single-writer `flock` rule (one process per database, D18) must hold across
every new surface. We also deliberately ship **no** polyglot client SDKs in
this repo.

## Decisions

1. **One server, one bind, one auth, one shutdown path (D38)** — a single
   `serve` process owns the store and exposes the Streamable-HTTP MCP server
   at `POST /mcp` and the JSON API under `/api/v1/...` on the same axum 0.8
   router and TCP bind. `serve --stdio` is a second transport mode with no
   port or token. There is one bearer-auth middleware for both HTTP surfaces
   and one shutdown path; every writing CLI command still refuses a live
   store with `DbLocked` (D18).

2. **MCP is the tool surface; one transport-neutral API is the authority** —
   the eight MCP tools (`remember`, `recall`, `forget`, `pin`, `unpin`,
   `summarize`, `explain`, `status`) are thin wrappers: each maps its typed input to
   `src/api::MemoryApi` and formats the returned JSON as both structured
   content and fenced text (`src/mcp/mod.rs`). The JSON routes call the *same*
   `MemoryApi` (`src/server/json.rs`), so the business rules — budget guard,
   `provider = 'none'` degradation, trust derivation — exist in exactly one
   place instead of being reimplemented per transport. rmcp 3.4 is the
   transport/tooling layer (official Rust SDK, MSRV-compatible).

3. **Bearer auth, fail-closed by bind (D29)** — a token is required to start a
   non-loopback bind; a loopback bind may be tokenless. Requests without the
   correct `Authorization: Bearer <token>` get 401 before any routing. `GET
   /healthz` is the one public, unauthenticated probe (no store access).

4. **Docs-only clients, no SDKs in-repo (D39)** — clients are verified
   snippets (`docs/examples/`: curl, MCP-over-curl, Python `httpx`, MCP client
   config) plus the OpenAPI spec. SDKs are a separate repo or v0.6.0+.

5. **The OpenAPI spec is hand-written and coverage-tested** — no `utoipa`
   dependency; `tests/http_api.rs` walks the spec and asserts every documented
   route exists (and that the route set matches the router), so the spec cannot
   silently drift from the implementation.

6. **Packaging is a musl static binary + a tiny container (0006/0007)** —
   `make build-musl` produces an `x86_64-unknown-linux-musl` static binary;
   the Docker image runs it on `alpine:3.20` (musl-native) with `serve` as the
   entry point. Compose binds off-loopback, so a token is mandatory
   (fail-closed). *Deviation: the runtime is alpine, not
   `distroless/static-debian12` — that base ships no shell or tools, so the
   compose `/healthz` healthcheck would be impossible.* Because the image build
   performs the musl release build in its builder stage, building the image is
   the only end-to-end proof of both 0006 and 0007; that is why the `docker` CI
   job and `make docker-smoke` exist rather than a docs-only claim.

## Consequences

- One process still owns a database; clients call it over the wire, and the
  lock rule keeps `remember`/`recall` etc. out of a live server's DB file.
- The JSON API and MCP share the `/healthz` probe, the auth header semantics,
  and the exit-code ↔ HTTP-status ↔ JSON `code` error taxonomy documented in
  `docs/interfaces.md`.
- A future concern can expose a new surface (for example a gRPC client)
  on the same router without new auth or lifecycle machinery.
- Alpine-over-distroless is a small surface-area increase (BusyBox if needed)
  traded for an actually-verifiable healthcheck; revisit if a distroless base
  gains a usable tool.