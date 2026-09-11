# Hermes agent memory integration

Hermes is a lightweight on-device agent runtime. This guide wires Hermes to a
self-hosted agos-memory instance over MCP. Both transports are shipped in
v0.5.0; pick one per deployment — never both against the same database:

- **stdio** — Hermes launches `agos-memory serve --stdio` as a subprocess and
  speaks newline-delimited JSON-RPC on stdin/stdout. No port, no token, no
  network: the right choice for a single-agent box.
- **Streamable HTTP** — one long-lived `agos-memory serve` process that several
  Hermes workers (and dashboards) share at `http://127.0.0.1:8710/mcp`. The
  same process also serves the JSON API at `/api/v1` (D38).

The full interface reference — tools, routes, auth, error taxonomy, exit
codes — is [docs/interfaces.md](../interfaces.md). Raw wire recipes live in
[`docs/examples/`](../examples/).

## Prerequisites

- The `agos-memory` binary on the same host (built with `make build`, or
  installed).
- A Hermes runtime with MCP tool providers.
- An **absolute** `db_path` in the config, so the database does not depend on
  the working directory the client happens to use (see §6).

## 1. One-time setup

Initialize the store:

```bash
agos-memory --config /abs/path/agos-memory.toml init
```

A minimal config (`/abs/path/agos-memory.toml`):

```toml
db_path = "/abs/path/memories.db"
agent_id = "hermes-prod"

[embed]
# "openai_compat" (default) needs a reachable /v1/embeddings endpoint;
# "hash" is the deterministic offline stub used by tests/eval;
# "none" degrades recall to FTS5 keyword search only.
provider = "openai_compat"
# base_url = "http://127.0.0.1:8080"   # agos-proxy; model/api_key as needed

[llm]
# Needed by summarize (and extraction); any OpenAI-compatible /v1/chat/completions.
# base_url = "http://127.0.0.1:8080"
# model = "gpt-4o-mini"

[server]
# Only for the HTTP transport; omit for stdio-only deployments.
# bind = "127.0.0.1:8710"
# token = "at-least-16-characters"
```

`provider = "hash"` is a lexical similarity proxy: deterministic, offline, and
useful for tests — not a production embedding model.

## 2. Wire Hermes as an MCP stdio tool provider

Hermes should spawn

```bash
agos-memory serve --stdio --config /abs/path/agos-memory.toml
```

and speak MCP over stdin/stdout. The process exits cleanly on stdin EOF and
logs to stderr, so a client may route stderr straight to its own log sink.
Most MCP clients take that command line as a JSON config entry — see
[`docs/examples/claude-desktop.json`](../examples/claude-desktop.json) for the
shape (`command` + `args`; `env` for `AGOS_MEMORY_*` overrides).

The stdio roundtrip is proven by `tests/mcp_stdio.rs`, which spawns the real
binary and drives
`initialize → tools/call remember → tools/call recall → explain → status`. Any
MCP-compliant client works because the wire protocol is stock MCP (rmcp 3.4).

## 3. Wire Hermes as an MCP HTTP client

Run the server (default transport is HTTP):

```bash
agos-memory serve --config /abs/path/agos-memory.toml
# logs: "HTTP server listening on 127.0.0.1:8710 (MCP + JSON API)"
```

Hermes then connects to:

- MCP (Streamable HTTP, SEP-2567): `http://127.0.0.1:8710/mcp`
- JSON API: `http://127.0.0.1:8710/api/v1/...`
- liveness (public, no auth): `http://127.0.0.1:8710/healthz`

The MCP client must send `Accept: application/json, text/event-stream` on every
POST, keep the `mcp-session-id` returned by `initialize`, and send it back on
every later request — the raw sequence is spelled out in
[`docs/examples/streamable-http.sh`](../examples/streamable-http.sh).
`--bind` and `--token` override `[server]`.

## 4. Tool surface

Eight tools, identical over both transports (issue 0001 plus the pin follow-up):

| Tool        | Arguments (required in **bold**)                    | When to call                                                  |
|-------------|-----------------------------------------------------|----------------------------------------------------------------|
| `remember`  | **`text`**, `tier`, `kind`, `source_kind`, `confidence` | After a notable user event or a conclusion the agent reached  |
| `recall`    | **`text`**, `k`, `budget_tokens`, `include_untrusted`, `include_pending`, `include_episodic` | Before answering, to ground the response in stored memory |
| `forget`    | **`id`**, `action` (`soft`\|`restore`\|`hard`\|`rollback`), `to_version`, `reason` | When a memory is superseded, wrong, or privacy-sensitive |
| `summarize` | `id`, `tier`, `all`, `force`                          | On session close, or on demand for long histories             |
| `explain`   | **`id`**                                             | When the agent must show its work (provenance, citations)      |
| `status`    | —                                                    | Dashboards / health checks                                     |
| `pin`       | **`id`**                                             | Make an explicit memory win retrieval-budget priority          |
| `unpin`     | **`id`**                                             | Remove that priority without changing trust or status          |

Every tool returns one text part (the human-readable rendering) plus
`structuredContent` with the machine-readable payload. Caller mistakes come
back as JSON-RPC errors (`INVALID_PARAMS`), store problems as
`INTERNAL_ERROR` — never with store internals or secrets in the message.

## 5. Auth

- Loopback binds (`127.0.0.1`, `localhost`, `::1`) may run **without** a token;
  that is the local-development mode.
- Any non-loopback bind **requires** a token of at least 16 characters — the
  server refuses to start without one (fail closed, D11/D38), and refuses it
  again per request as defence in depth.
- With a token configured, **every** `/mcp` and `/api/v1` request needs
  `Authorization: Bearer <token>` (constant-time compare); `/healthz` stays
  public. Missing or wrong credentials answer `401` with no hint about the
  expected value.

Give Hermes the token through the config file (`[server] token`), the
`AGOS_MEMORY_TOKEN` environment variable, or `serve --token`.

## 6. One process, one database

`serve` opens the store once and holds the single-writer `flock` for its whole
lifetime. A second process that needs the store — `remember`, `recall`,
`session`, `backup`, another `serve` — refuses to start with `DbLocked`
(exit code 4) and names the holding pid. The read-only commands `status` and
`doctor` still work against a live server.

Consequences for Hermes deployments:

- One `serve` process per database file, however many Hermes workers talk to it.
- `agos-memory backup --out <file>` needs the server **stopped** (it takes the
  lock); see the [runbook](../operations/runbook.md) for the snapshot and
  restore procedure.
- A relative `db_path` resolves against the *process* working directory, so a
  client that launches `serve` from an arbitrary cwd can silently create a
  second, empty database. Always use an absolute `db_path` (or pass
  `--db /abs/path/memories.db`).

## 7. Trust policy

Recall defaults to **Strict** (D29): only `trusted`/`system` memories are
eligible. `remember` assigns trust from `source_kind` — `tool`, `web`,
`import`, and `file` are forced to `untrusted`; only `user` and `agent` are
trusted. Untrusted memories are fenced data: they are returned only when the
caller passes `include_untrusted: true`, and they are never re-labelled as
trusted.

Two more opt-ins matter for Hermes:

- `include_episodic: true` — `remember` defaults to the **episodic** tier, and
  episodic memories are excluded from recall unless the query opts in (D26).
- `include_pending: true` — low-confidence writes land in `pending` and are
  hidden until explicitly requested (D28).

## 8. Suggested recall cadence

- **Pre-response**: one `recall` with the user's latest utterance as the query
  text, `k = 5`, plus `include_episodic: true` if the agent stores episodic
  memories (the default tier for `remember`). Add `budget_tokens` when the
  context window is tight — packing guarantees the result stays within it.
- **Post-response**: one `remember` with a condensed fact from the exchange.
  Store one sentence per fact, not the transcript: dedup keeps near-identical
  text from piling up, and summaries are generated from individual memories.

## 9. Observability

- `GET /healthz` — public liveness probe (`{"status": "ok"}`), safe for
  Hermes-side health checks and container/compose healthchecks.
- `status` tool / `GET /api/v1/status` — `schema_version`, `agent_id`, one
  `{status, count}` bucket per memory status, and `embeddings_cache`
  (`entries`/`hits`/`misses`).
- `agos-memory doctor` — schema, sqlite-vec, FTS5, integrity, foreign keys.
- The pinned embedding dimension lives in `meta`
  (`select value from meta where key = 'embed_dim'`); recall skips the vector
  leg (and reports `degraded: true`) when the embedder cannot produce a
  matching vector.

## 10. Troubleshooting

- **No memories returned by recall** — the three common causes are the
  opt-ins: episodic (`include_episodic`), pending (`include_pending`), and
  trust (untrusted memories need `include_untrusted`). Then check
  `min_score`/`k` and that the query text overlaps the stored text.
- **`degraded: true` or all hits missing** — `provider = "none"` has no
  vectors (keyword-only); `provider = "hash"` is a lexical stub. Both are
  offline modes; production quality needs `openai_compat` with a reachable
  endpoint.
- **`summarize` fails with `code = "LLM"`** — `[llm] base_url` is
  unreachable. The route answers HTTP 502 with the provider error; nothing is
  written.
- **`DbLocked` / exit code 4** — another process holds the database. The
  message names the holding pid; stop it, or point the new process at a
  different `--db`.
- **Tool not found / schema mismatch** — the client is connected to a server
  binary of a different version. Tool names are stable across v0.5.x; restart
  both sides on the same build.
- **A second, empty database appears** — the server was started from another
  working directory with a relative `db_path`. Make `db_path` absolute (§6).
