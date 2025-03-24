# Operations

## Runbook

### Health check

```sh
agos-memory doctor   # schema, sqlite-vec, fts5, integrity, foreign_keys
agos-memory status   # memory counts per status
```

### One process per database

Only one agos-memory process may open a database at a time. A second process
fails with exit code 4 and the holding pid. If a process was killed while
holding the lock, the lock is reclaimed automatically (stale-pid detection).

### Upgrade

1. Stop the running process.
2. Replace the binary.
3. Run `agos-memory doctor` — schema version is checked against the binary.
   Databases from newer versions refuse to open (exit code 3).

## Backups

- Online snapshot: `VACUUM INTO '/backups/memory-<date>.db'` (planned CLI
  wrapper: `agos-memory export --snapshot` in v0.6.0).
- Offline copy: stop the process, copy the `.db` file (WAL file is empty at
  clean shutdown).

## Monitoring

- `llm_calls` table: cost per day/purpose (`SELECT date(created_at/1000,'unixepoch'), SUM(total_tokens) ...`).
- `recalls` table: latency (`latency_ms`), no-hit rate (`no_hit`).
- `jobs_dead` table: non-empty means extraction failures need attention.

## Log filters

`AGOS_MEMORY_LOG=debug agos-memory doctor` (same syntax as RUST_LOG).

## Exit codes

| Code | Meaning |
|------|---------|
| 0    | success |
| 2    | config or input error |
| 3    | database schema too new |
| 4    | database locked by another process |
| 5    | storage error |
| 6    | embedding provider error / mismatch |
| 7    | LLM error |
| 8    | session token ceiling exceeded |
