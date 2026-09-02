# Interfaces — transports, tools, routes, auth, errors

The complete agos-memory v0.6.0 interface reference describes what clients
can call and how failures are reported on each surface. Three normative/verified
companions live alongside this page:

- [`docs/api/openapi.yaml`](api/openapi.yaml) — normative JSON API schemas
  (route-coverage tested in `tests/http_api.rs`, issue 0002).
- [`docs/examples/`](examples/) — every shape below, runnable end to end
  (curl, MCP-over-curl, Python `httpx`, MCP client config).
- [`docs/operations/runbook.md`](operations/runbook.md) — operator procedures,
  including the same exit-code table.

Per-agent walkthroughs: [Hermes](integrations/hermes.md),
[OpenClaw](integrations/openclaw.md).

## 1. Transports

One `serve` process exposes up to two surfaces on a single bind, with one auth
layer and one shutdown path (D38):

| Surface | Address | Use |
|---------|---------|-----|
| MCP Streamable HTTP | `POST /mcp` | default; several clients share one server |
| JSON API | `/api/v1/...` | dashboards, ingestion scripts, plain HTTP clients |
| Liveness | `GET /healthz` | public probe (no auth, no store access) |
| MCP stdio | stdin/stdout | `serve --stdio`; single-agent, no port/token |

```bash
agos-memory serve --config /abs/path/agos-memory.toml           # HTTP
agos-memory serve --stdio   --config /abs/path/agos-memory.toml # stdio
# flags override config: --bind, --token
```

Startup logs `HTTP server listening on <bind> (MCP + JSON API)` on the HTTP
transport. The stdio transport logs to stderr and exits cleanly when stdin
closes (EOF).

### Streamable HTTP wire rules (rmcp 3.4)

Each rule below is exercised verbatim by
[`docs/examples/streamable-http.sh`](examples/streamable-http.sh):

1. `POST /mcp` with `Content-Type: application/json` **and**
   `Accept: application/json, text/event-stream` — rmcp answers **406**
   without the combined `Accept`.
2. Responses are SSE streams; the JSON-RPC payload is the non-empty `data:`
   line carrying an `id`.
3. `initialize` returns an `mcp-session-id` header; every later request **must**
   echo it. Without it the server treats the request as a fresh session and
   answers `Unexpected message, expect initialize request`.
4. After `initialize`, send `notifications/initialized` (→ 202). Requests may
   carry `mcp-protocol-version: 2025-03-26`.
5. Tool calls return one text part (the CLI rendering) **plus**
   `structuredContent` — the machine-readable payload.

The stdio transport speaks the same MCP protocol as newline-delimited JSON-RPC
(no SSE, no session headers). The Rust interop proofs are
`tests/mcp_stdio.rs` (spawns the real binary) and `tests/mcp_http.rs`.

## 2. MCP tools (six, identical on both transports)

Arguments in **bold** are required. Defaults are applied server-side.

| Tool | Arguments | Defaults / notes |
|------|-----------|------------------|
| `remember` | **`text`**, `tier`, `kind`, `source_kind`, `confidence` | `tier=episodic`, `kind=fact`, `source_kind=agent`, `confidence=1.0`; `tool`/`web`/`import` sources are forced `untrusted` |
| `recall` | **`text`**, `k`, `budget_tokens`, `include_untrusted`, `include_pending`, `include_episodic` | packing guarantees `tokens_used <= budget_tokens` (D25) |
| `forget` | **`id`**, `action`, `to_version`, `reason` | `action=soft` — one of `soft`\|`restore`\|`hard`\|`rollback`; `to_version` required for `rollback` |
| `summarize` | `id`, `tier`, `all`, `force` | by id, by tier, or `all=true`; needs a reachable `[llm]` |
| `explain` | **`id`** | provenance: source, links, ref_count, recall history |
| `status` | — | `schema_version`, `agent_id`, `counts[]` (**one `{status, count}` bucket per memory status**), `embeddings_cache` |

Every tool answers `{ text, structuredContent }`. Caller mistakes (empty
`text`, unknown id, bad action) surface as JSON-RPC `INVALID_PARAMS` with an
actionable message; store failures surface as `INTERNAL_ERROR` with a
redacted message (see §5).

## 3. JSON API routes

Normative schemas: [`docs/api/openapi.yaml`](api/openapi.yaml). The bodies are
field-for-field the MCP tools' `*Input`/outcome shapes.

| Method | Path | Body → outcome |
|--------|------|----------------|
| `GET`  | `/healthz` | `{"status": "ok"}` — public |
| `POST` | `/api/v1/remember` | `RememberInput` → created memory (`public_id`, `status`, `trust`) |
| `POST` | `/api/v1/recall` | `RecallInput` → `RecallReport` (`hits[]`, `tokens_used`, `degraded`) |
| `POST` | `/api/v1/forget/{id}` | `{action, to_version?, reason?}` → action outcome |
| `POST` | `/api/v1/summarize` | `{id? , tier?, all?, force?}` → `{scope, count, summaries[]}` |
| `GET`  | `/api/v1/explain/{id}` | explain report — the id is a **path** parameter |
| `GET`  | `/api/v1/status` | `schema_version`, `agent_id`, `counts[]`, `embeddings_cache` |

Working recipes for every route: [`docs/examples/remember-recall.sh`](examples/remember-recall.sh)
(curl) and [`docs/examples/python-httpx.py`](examples/python-httpx.py) (Python).

## 4. Auth

Fail-closed policy (D11), enforced twice — at startup by `Config::validate()`
and per request by the bearer middleware (`src/server/auth.rs`):

- **Token configured** (`[server] token`, `AGOS_MEMORY_TOKEN`, or `--token`):
  every `/mcp` and `/api/v1` request must send
  `Authorization: Bearer <token>` (constant-time compare). Anything else
  answers **401** with a plain-text reason that never echoes the expected
  value.
- **No token**: only a **loopback** bind may serve (local development/CI). A
  non-loopback bind without a token fails startup; a non-loopback bind with a
  token shorter than **16 characters** also fails startup. The bind address is
  the authority — `Host`/`Origin` headers are never consulted.
- **`GET /healthz`** is always public.

## 5. Error taxonomy

The same variant vocabulary appears on every surface: process exit code
(CLI), `{error, code}` body + HTTP status (JSON API), and JSON-RPC error
(MCP). Source of truth: `src/main.rs`, `src/server/api.rs`, `src/mcp/mod.rs`.

| Variant | Exit | HTTP | `code` | JSON-RPC |
|---------|------|------|--------|----------|
| `Config` / `InvalidInput` | 2 | 400 | `CONFIG` / `INVALID_INPUT` | `INVALID_PARAMS` (message passed through) |
| `MemoryNotFound` | —¹ | 404 | `NOT_FOUND` | `INVALID_PARAMS` with actionable message (`memory <id> not found`) |
| `SchemaTooNew` | 3 | 503 | `SCHEMA_TOO_NEW` | `INTERNAL_ERROR` (redacted) |
| `DbLocked` | 4 | 503 | `DB_LOCKED` | `INTERNAL_ERROR`, message passed through (names the holding pid) |
| `Storage` | 5 | 500 | `STORAGE` | `INTERNAL_ERROR` (redacted) |
| `Embedder` / `EmbeddingMismatch` | 6 | 502 | `EMBEDDER` / `EMBEDDING_MISMATCH` | `INTERNAL_ERROR` (redacted) |
| `Llm` | 7 | 502 | `LLM` | `INTERNAL_ERROR` (redacted) |
| `BudgetExceeded` | 8 | 429 | `BUDGET_EXCEEDED` | `INVALID_PARAMS`, message passed through (names used/ceiling) |
| success | 0 | 200 | — | result |

Redaction rule (D21 lineage): MCP clients only ever see the curated, secret-free
`Display` of `InvalidInput`, `BudgetExceeded`, and `DbLocked`; everything else
is the literal string `internal error`. Store internals and secrets never
cross the wire.

¹ `MemoryNotFound` only escapes on the JSON API (404). CLI paths map
not-found differently: `forget` reports `InvalidInput` (exit 2) and `explain`
exits `1` directly, before the taxonomy applies.

Auth failures are **401** on both HTTP surfaces (they never reach a route
handler, so they carry a plain-text reason, not the `{error, code}` body).

### Budgets

Two distinct ceilings:

- **`budget_tokens` per recall** — packing places hits within it; the report
  states `tokens_used` (always `<= budget_tokens`, D25).
- **`[budget] max_tokens_per_session`** — attributed provider-token ceiling
  (0 = unlimited). At or above the ceiling, further work for that session
  returns `BudgetExceeded` (exit 8 / HTTP 429 / MCP `INVALID_PARAMS`). Only
  ledger rows with a real `session_id` are charged; the extractor enforces it
  before the LLM call. Run `agos-memory cost --session <public-id>` for the
  used/remaining/over-budget view. See [observability.md](observability.md).

## 6. Trust and opt-in filters (D26/D28/D29)

- Recall is **Strict** by default: only `trusted`/`system` memories are
  eligible.
- `remember` derives trust from `source_kind`: `tool`, `web`, `import`, and
  `file` → `untrusted`; only `user` and `agent` → `trusted`.
- Opt-ins are per recall call and never relabel a memory:
  `include_untrusted` (fenced data), `include_episodic` (episodic tier is
  opt-in), `include_pending` (below `pending_threshold`).

## 7. One process, one database (D18)

`serve` (and every writing CLI command) holds an exclusive `flock` on the
database for its lifetime. A second process that needs the store refuses to
start with `DbLocked` (exit 4, names the holding pid). `agos-memory status`
(opens read-only) and `agos-memory doctor` still work while a server runs;
`backup` requires the server to be stopped. Details:
[runbook](operations/runbook.md).

## 8. Configuration and environment

Precedence (lowest first): built-in defaults → TOML file (`--config`,
default `./agos-memory.toml`) → `AGOS_MEMORY_*` environment → CLI flags.

Common `AGOS_MEMORY_*` overrides: `AGOS_MEMORY_DB_PATH`,
`AGOS_MEMORY_AGENT_ID`, `AGOS_MEMORY_LOG` (RUST_LOG-style filter),
`AGOS_MEMORY_BIND`, `AGOS_MEMORY_TOKEN`, `AGOS_MEMORY_EMBED_BASE_URL`,
`AGOS_MEMORY_EMBED_MODEL`, `AGOS_MEMORY_EMBED_API_KEY`,
`AGOS_MEMORY_LLM_BASE_URL`, `AGOS_MEMORY_LLM_MODEL`,
`AGOS_MEMORY_LLM_API_KEY`, `AGOS_MEMORY_RECALL_TOP_K`,
`AGOS_MEMORY_RECALL_BUDGET_TOKENS`, `AGOS_MEMORY_RECALL_MIN_SCORE`,
`AGOS_MEMORY_RECALL_INCLUDE_{EPISODIC,PENDING,UNTRUSTED}`.

Keep `db_path` **absolute**: a relative path resolves against the *process*
working directory, and a client that launches the server from another cwd
would silently open a different database.
