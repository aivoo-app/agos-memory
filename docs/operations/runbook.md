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

## Retention & forgetting (v0.4.0)

Full reference: `docs/forget.md`. Operator quick reference:

```sh
agos-memory forget soft <pid>            # deprecate (recoverable)
agos-memory forget restore <pid>         # undo a soft deprecate
agos-memory forget hard <pid> --reason   # verified purge + VACUUM + tombstone
agos-memory forget rollback <pid> --to-version <n>
agos-memory forget list-tombstones
agos-memory forget list-audit --action ttl_purge --limit 20
agos-memory maintain --ttl               # run the TTL reaper now
agos-memory maintain --consolidate       # summarize pass + dedup + orphan cleanup
agos-memory maintain --schedule          # foreground scheduler (reaper_hour, [consolidate])
agos-memory summarize --id <pid> --force
```

- **Hard purges are final.** The tombstone row is the only residue, and it is
  append-only (schema-v4 triggers abort any UPDATE/DELETE). Verify a purge by
  replaying the tombstone row counts against `status`.
- **Automated vs manual actions** are distinguishable in `list-audit`
  (`ttl_deprecate`/`ttl_purge` come from the reaper; requester `ttl-reaper`).
- **Retention knobs** live under `[memory]` (`ttl_<tier>_days`,
  `ttl_grace_days`, `reaper_hour`) and `[consolidate]` (day/hour). Nothing is
  auto-purged before the grace period elapses.
- **Backups and purges interact**: a tombstone exists only in the live DB.
  Restoring an old snapshot resurrects the memory rows a purge removed —
  re-run `forget hard` after restoring if the deletion must be preserved.

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
