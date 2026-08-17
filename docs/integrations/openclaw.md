# OpenClaw memory integration

OpenClaw writes agent memory to `MEMORY.md` and daily logs under
`memory/YYYY-MM-DD.md`. `agos-memory ingest` imports that tree through the
normal redacted, embedded, deduplicated write path so it is queryable through
the JSON API and MCP tools after OpenClaw restarts.

## Quick start

Stop the writer/serve process for the database before running the CLI command;
one process owns the database at a time.

```sh
agos-memory --config /abs/path/agos-memory.toml init
agos-memory --config /abs/path/agos-memory.toml ingest /abs/path/openclaw
```

Use a throwaway database for a dry run:

```sh
agos-memory --config /abs/path/agos-memory.toml \
  ingest /abs/path/openclaw --dry-run
```

The command accepts either the OpenClaw directory or one Markdown file. A
directory discovers `MEMORY.md` and valid `memory/YYYY-MM-DD.md` files in
lexicographic order.

## Parsing and provenance

Headings and blank lines are skipped. Each list item or paragraph becomes one
memory:

- `source_kind = "file"`
- `source_ref = "MEMORY.md:12"` or
  `source_ref = "memory/2026-09-24.md:8"`
- `tier = "semantic"`, `kind = "fact"`, `confidence = 0.9`

Daily-file items are also recorded in the normal sessions/turns log. The source
reference is stored as provenance metadata; it is not appended to the memory
text. A manifest in the store's `meta` table maps each `file:line` to a stable
hash and public id.

An unchanged tree is a no-op:

```text
ingest seen=3 new=0 updated=0 unchanged=3 stale=0 skipped=0
```

Editing one item updates only that memory's version. Removing a line or file
reports it as `stale=N`; the memory is retained for operator review and must be
explicitly forgotten with the normal lifecycle command. This prevents a source
deletion from silently becoming a deletion in the memory database.

## Trust policy

`file` is **untrusted by default**. OpenClaw may write files derived from web
pages, tools, fetched documents, or other agent output, so file provenance must
not mint trusted instructions. Strict recall therefore excludes these rows.
Callers that intentionally consume them pass `include_untrusted: true`; the
consumer receives fenced data with `trust="untrusted"` and must not execute it
as instructions.

This decision is part of D46 and applies to every write path, not only the
ingest command. A later trusted edit, pin, import, dedup collision, rollback,
or summary cannot upgrade an untrusted row.

## Querying

```sh
BASE=http://127.0.0.1:8710
TOKEN=your-token

curl -sS -X POST "$BASE/api/v1/recall" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"text":"deploy schedule","k":10,"include_untrusted":true}' | jq .
```

Use `explain` to inspect the stored source kind and `source_ref`:

```sh
curl -sS "$BASE/api/v1/explain/<public_id>" \
  -H "Authorization: Bearer $TOKEN" | jq .
```

## Running against a live server

`serve` owns the database writer lock for its lifetime. A CLI `ingest` against
a live server correctly fails with `DbLocked` (exit 4). Stop the server and run
the command, or keep using the JSON API from a client while the server owns the
store. The API does not bypass trust or provenance rules.

## Troubleshooting

- **No Strict recall results** — file memories are untrusted by design; use
  `include_untrusted: true` only in a consumer that renders fenced data.
- **Duplicate-looking rows after editing** — inspect the manifest/source refs
  and review the old row; edits create a new version, while unrelated duplicate
  content may be handled by the normal dedup policy.
- **`DbLocked`** — stop `serve` before running the CLI ingest command.
- **Embedding provider error** — configure `[embed]` or use
  `provider = "none"` for the documented keyword-only degraded path. Secrets
  are redacted before either storage or embedding.
- **Unexpected item count** — headings and blank lines are skipped; list items
  and paragraphs are the documented granularity.

For the complete CLI surface, see [docs/cli.md](../cli.md). For API transport
and trust details, see [docs/interfaces.md](../interfaces.md). For deletion and
restore behavior, see [docs/forget.md](../forget.md).
