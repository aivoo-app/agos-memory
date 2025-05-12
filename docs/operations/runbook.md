# Operations

## Runbook

### Health check

```sh
agos-memory doctor   # schema, sqlite-vec, fts5, integrity, foreign_keys
agos-memory status   # memory counts per status
```

### One process per database

Only one agos-memory process may open a database at a time. A second process
fails with exit code 4 and names the holding pid. The lock is an exclusive
non-blocking `flock` on a `<db>.lock` sidecar: the kernel releases it when the
holder dies, so a lock left behind by a killed process is reclaimed
automatically on the next open. The sidecar file itself may remain after a
clean shutdown — it is empty and harmless (git-ignored via `*.lock`); do not
delete it while a process is running.

### Upgrade

1. Stop the running process.
2. Replace the binary.
3. Run `agos-memory doctor` — schema version is checked against the binary.
   Databases from newer versions refuse to open (exit code 3).

## Backups

- Online snapshot: `agos-memory backup --out /backups/memory-<date>.db` —
  `VACUUM INTO` through the single writer, then verified (`integrity_check`
  must be `ok` and core-table row counts must match the live database). Takes
  the process lock: stop the running server first, or run the command from a
  dedicated invocation.
- Restore: stop any process using the database, replace the `.db` file with
  the snapshot, run `agos-memory doctor` (schema + integrity checks).
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
