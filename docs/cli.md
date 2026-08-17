# CLI reference

## `ingest`

```sh
agos-memory ingest <dir-or-file> [--dry-run]
```

Ingests the OpenClaw Markdown layout:

- `MEMORY.md`
- `memory/YYYY-MM-DD.md`

A list item or paragraph becomes one memory with `source_kind = "file"` and a
`source_ref` such as `memory/2026-09-24.md:12`. Headings and blank lines are
skipped. File-sourced memories are untrusted by default because an agent may
have written them from fetched or tool-derived content. They are excluded from
Strict recall and are returned as fenced data only with
`--include-untrusted` on recall.

The normal write path redacts secrets before embedding and storage. Daily
Markdown items are also appended to the store's session/turn log. The ingest
manifest is stored in the `meta` table, keyed by `file:line` and content hash:

- unchanged tree: `new=0`, `updated=0`, `unchanged=N`;
- edited item: a new memory version is recorded;
- deleted source item: reported as `stale=1` and retained for explicit
  operator review/forget;
- `--dry-run`: parses and reports counts without writing memories or manifest
  changes.

Example:

```sh
agos-memory ingest ~/openclaw --dry-run
agos-memory ingest ~/openclaw
```

Run it while no other process owns the database. A `serve` process holds the
single-writer lock; ingest through the command when the server is stopped, or
use the normal API write path from a client while the server is running.

## Other write commands

- `remember --text ... [--tier ...] [--kind ...] [--source-kind ...]`
- `session open|append|close|idle-close`
- `import [--dry-run] <jsonl>`

All normal write commands use provenance-aware trust. Tool, web, import, and
file sources cannot mint trusted memories.
