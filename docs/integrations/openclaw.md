# OpenClaw memory integration

OpenClaw writes agent memory to `MEMORY.md` and daily logs in
`memory/YYYY-MM-DD.md`. This guide ingests those files into agos-memory so the
agent's memory is queryable over the JSON API and MCP tools and survives
OpenClaw restarts.

## Overview

The ingestion path is: OpenClaw markdown files → `remember` calls → memory
store. Each ingested line becomes one memory with `source_kind = "file"`
(trusted, see §6). To keep a line traceable to its origin, put the reference
**in the text** — the write API takes `text`, `tier`, `kind`, `source_kind`,
and `confidence`, and nothing else:

```
memory: 4f1c… (tier=episodic, kind=fact, source_kind=file)
text:    "Deploys go out on Thursdays. [source: MEMORY.md:12]"
```

The interface reference is [docs/interfaces.md](../interfaces.md); the raw API
recipes are in [`docs/examples/`](../examples/).

## 1. One-time setup

Initialize a store:

```bash
agos-memory --config /abs/path/agos-memory.toml init
```

Config:

```toml
db_path = "/abs/path/openclaw-memories.db"   # absolute: see §6
agent_id = "openclaw-prod"

[embed]
provider = "openai_compat"   # needs a reachable /v1/embeddings endpoint

[llm]
# Needed by summarize (session close / consolidation).
# base_url = "http://127.0.0.1:8080"
# model = "gpt-4o-mini"

[server]
bind = "127.0.0.1:8710"
token = "at-least-16-characters"
```

Start the server:

```bash
agos-memory serve --config /abs/path/agos-memory.toml
```

`serve` holds the single-writer lock for its lifetime, so run every ingestion
step as an API client (§2) rather than as a second CLI process on the same
database — a `remember` invocation against a live server refuses with
`DbLocked` (exit 4).

## 2. Ingestion recipe

Save as `ingest.sh` and run it once per file:

```bash
#!/usr/bin/env bash
# Ingest one OpenClaw markdown file into agos-memory (JSON API).
# Every non-empty, non-heading line becomes one memory with source_kind = "file".
set -euo pipefail

BASE="${BASE:-http://127.0.0.1:8710}"
TOKEN="${TOKEN:-your-token}"
AUTH=(-H "Authorization: Bearer $TOKEN")
FILE="${1:?usage: ingest.sh <markdown-file>}"
STORE_ID="$(basename "$FILE")"

line=0
ingested=0
while IFS= read -r text; do
    line=$((line + 1))
    # Skip blank lines and headings.
    [ -n "$text" ] || continue
    case "$text" in \#*) continue ;; esac

    # jq builds the body, so quotes/newlines in the text cannot break the JSON.
    body=$(jq -nc --arg t "$text [source: ${STORE_ID}:${line}]" \
        '{text: $t, source_kind: "file", tier: "semantic", kind: "fact", confidence: 0.9}')

    curl -sS -X POST "$BASE/api/v1/remember" \
        -H 'Content-Type: application/json' "${AUTH[@]}" -d "$body" > /dev/null
    ingested=$((ingested + 1))
done < "$FILE"

echo "ingested $ingested line(s) from $FILE"
```

Then:

```bash
BASE=... TOKEN=... ./ingest.sh /path/to/MEMORY.md
BASE=... TOKEN=... ./ingest.sh /path/to/memory/2026-09-22.md
```

Notes:

- `tier = "semantic"` is deliberate: OpenClaw's `MEMORY.md` holds durable
  facts, and semantic memories are recall-eligible by default, while
  `episodic` (the `remember` default) needs `include_episodic: true`.
- `confidence = 0.9` keeps writes above the pending threshold; lower values
  park a memory in `pending` until `include_pending` is passed.
- Headings and blank lines are skipped, so the ingested count can be lower
  than the file's line count.

## 3. Idempotency

Re-running the script over the same file is safe:

- With a vector embedder, near-identical text is deduplicated at insert time
  (cosine similarity ≥ 0.92): the existing row's `ref_count` is bumped instead
  of a second row being created.
- In keyword-only mode (`provider = "none"`) dedup falls back to an exact
  SHA-256 text hash.
- Edited lines have different text, so they insert as new memories (and, when
  they supersede an older line, the old one stays until you `forget` it).

## 4. Querying ingested memories

```bash
# Recall by topic (semantic tier needs no opt-in).
curl -sS -X POST "$BASE/api/v1/recall" \
    -H 'Content-Type: application/json' -H "Authorization: Bearer $TOKEN" \
    -d '{"text": "deploy schedule", "k": 10}' | jq .

# Provenance drill-down: the id is a *path* parameter.
curl -sS "$BASE/api/v1/explain/<public_id>" -H "Authorization: Bearer $TOKEN" | jq .

# Health: counts per status + index cache.
curl -sS "$BASE/api/v1/status" -H "Authorization: Bearer $TOKEN" | jq .
```

The `[source: FILE:LINE]` suffix recorded in the text is what makes a hit
traceable back to the OpenClaw file; `explain` adds the store-side provenance
(`source_kind`, `source_ref`, `links`, inject history).

## 5. OpenClaw agent integration

If the OpenClaw runtime supports MCP tool providers, wire it to
`agos-memory serve --stdio` as described in
[docs/integrations/hermes.md](hermes.md): the agent can then `remember` and
`recall` directly, with no markdown round trip.

If the runtime only writes markdown files, run the ingestion script on a timer
(cron, systemd timer, or OpenClaw's own hooks). Two shapes are supported:

- **Client-side timer** (recommended while a server is running): call the JSON
  API as §2 does — any number of clients may share one `serve` process.
- **In-process jobs**: `agos-memory maintain --ttl`, `maintain --consolidate`,
  or `maintain --schedule`, which honors `[memory] reaper_hour` and
  `[consolidate]` (enabled by default, `day = 0` Sunday / `hour = 4` UTC).
  These need the store lock, so run them only while no `serve` process is up,
  or from a process that owns the database.

## 6. Trust policy

OpenClaw-written memories arrive as `source_kind = "file"`, which maps to
`trust = 'trusted'` — only `tool`, `web`, and `import` are forced to
`untrusted` (D29). If you ingest files from an untrusted source (a downloaded
dump, a scraped page), send `source_kind = "import"` or `"web"` instead: those
memories are fenced and surface only when the caller passes
`include_untrusted: true`.

## 7. Troubleshooting

- **Ingested lines never show up in recall** — check the tier: this recipe
  writes `semantic` (eligible by default), but anything written as `episodic`
  needs `include_episodic: true`. Also confirm the embedder: with
  `provider = "none"` only the FTS5 keyword leg runs, so a query of synonyms
  can miss.
- **Duplicates after editing a line** — dedup compares text, not provenance.
  Edit-and-reingest creates a second memory; `forget` the old one (soft, then
  hard) to keep the store clean.
- **`DbLocked` (exit 4) from a CLI ingestion loop** — a `serve` process holds
  the database. Ingest through the API (§2) or stop the server.
- **`code = "EMBEDDER"` / HTTP 502 while ingesting** — the embedding endpoint
  in `[embed] base_url` is unreachable; nothing is written for that line.
