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

## Static (musl) build

For minimal images (distroless/alpine in the Docker story) the default
release binary is too big and dynamically linked against glibc. Build a
statically linked `x86_64-unknown-linux-musl` binary with:

```sh
rustup target add x86_64-unknown-linux-musl   # one-time
# install musl-gcc (musl-tools):
#   Debian/Ubuntu: sudo apt-get install musl-tools
#   Arch (Garuda): sudo pacman -S musl
make build-musl
```

`build-musl` verifies both prerequisites first (clear error else), then:

```sh
cargo build --release --target x86_64-unknown-linux-musl
```

`[profile.release]` (`lto`, `strip`) is reused as-is, and the bundled
rusqlite/sqlite-vec C code compiles with `musl-gcc` — nothing is vendored.
The result is printed and `ldd` is checked to confirm "not a dynamic
executable". Runtime behavior is identical to the gnu binary; a statically
linked binary also crosses kernels without glibc (e.g. Alpine → the Docker
image below).

## Docker (0007)

```sh
export AGOS_MEMORY_TOKEN="$(openssl rand -hex 24)"   # >=16 chars, required
docker compose up --build        # builds the musl-static image (36 MB), serves on :8710
curl -s localhost:8710/healthz                        # liveness (no token)
curl -s -H "Authorization: Bearer $AGOS_MEMORY_TOKEN" localhost:8710/api/v1/status
```

The compose file binds off-loopback so the port is reachable; that is
fail-closed (D29) and **requires** `AGOS_MEMORY_TOKEN`. Runtime is
`alpine:3.20` (musl) running the statically-linked binary. Persistent store is
the named `agos_memory_data` volume at `/data/memory.db`; to add an embed/LLM
endpoint, uncomment the `AGOS_MEMORY_EMBED_*` / `AGOS_MEMORY_LLM_*` lines in
`docker-compose.yml`.

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
