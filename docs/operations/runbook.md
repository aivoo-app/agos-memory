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
- Restore: stop every process using the database before moving files. The
  restore sequence is intentionally offline because the product enforces one
  process per database:
  1. Preserve the current file for rollback: `mv memory.db memory.db.pre-restore`.
  2. With the process stopped, remove any leftover `memory.db-wal` and
     `memory.db-shm` files. Do not copy WAL/SHM files from another database.
  3. Keep `memory.db.lock` in place. On Unix it is an empty/stale sidecar and
     the kernel releases its `flock`; the next open reclaims it. Do not delete
     it while a process may still be running.
  4. Atomically install the verified snapshot at the configured live path:
     `cp /backups/memory-<date>.db memory.db.restore && mv memory.db.restore memory.db`.
  5. Run `agos-memory doctor` and `agos-memory status` using the same config/DB
     path. A successful restore must preserve schema version, embedding
     dimension, memory/tier/status counts, vectors, version history,
     `forget_audit`, and `tombstones`; then run a normal recall smoke query.
  6. If startup or verification fails, stop the process and move the preserved
     `memory.db.pre-restore` file back. Never start against a partially copied DB.
- The runbook procedure is exercised end to end by
  `cargo test --test restore_drill -- --nocapture --test-threads=1` (or
  `make restore-drill`). The test replaces the live file, reopens through
  `StoreHandle::open`, verifies product recall and purge state, proves a stale
  `.db.lock` does not deadlock, and proves a concurrent open fails with the
  database-lock error.
- A snapshot taken **before** a hard purge intentionally contains the deleted
  memory; snapshots cannot retroactively forget it. For deletion-preserving
  recovery, restore a post-purge snapshot. See [Retention & forgetting](#retention--forgetting-v040).
- Offline copy: stop the process, copy the `.db` file (WAL file is empty at
  clean shutdown).

## Monitoring

- `agos-memory cost [--session <id>] [--since <duration>] [--json]` reports
  provider tokens, estimated USD, per-purpose rows, `token_ledger` totals, and
  the per-session ceiling. See [observability](../observability.md).
- `llm_calls` table: cost per day/purpose (`SELECT date(created_at/1000,'unixepoch'), SUM(total_tokens) ...`).
- `recalls` table: latency (`latency_ms`), no-hit rate (`no_hit`).
- `jobs_dead` table: non-empty means extraction failures need attention.
- `make soak` runs the release mixed-traffic gate against a temporary database:
  remember/recall, session open/append/close plus extraction jobs, soft/hard
  forget, and TTL/full maintenance. It prints RSS, peak WAL, database growth,
  operations/second, and a final reconciliation table. The default duration is
  60 seconds; use `AGOS_SOAK_SECS=300 make soak` for a longer pre-release run.
  The test is ignored by ordinary `cargo test` and is also run by the push-only
  CI soak job. Linux RSS is bounded by default at 128 MiB growth; non-Linux
  hosts print a skip notice because `/proc/self/statm` is unavailable. Optional
  bounds can be overridden with `AGOS_SOAK_MAX_RSS_GROWTH_KIB`,
  `AGOS_SOAK_MAX_DB_GROWTH_BYTES`, and `AGOS_SOAK_MAX_WAL_BYTES`.
  `AGOS_SOAK_RETAIN_BYTES_PER_OP` is a mutation hook used to prove the RSS
  assertion fails; leave it unset in normal operation.

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

### Verifying the image

Building the image is the only end-to-end proof of the musl static build: the
builder stage runs the `x86_64-unknown-linux-musl` release compile, so a broken
musl toolchain or a missing source directory fails the build rather than
shipping a broken artifact. Two equivalent entry points:

```sh
make docker-smoke     # build + run the image, assert /healthz 200 and the auth matrix
```

```sh
docker build -t agos-memory:local .
CID=$(docker run -d -e AGOS_MEMORY_BIND=0.0.0.0:8710 \
      -e AGOS_MEMORY_TOKEN=smoke-token-0123456789 \
      -p 127.0.0.1:8710:8710 agos-memory:local)
# probe, then confirm the auth matrix (401 without a token, 200 with):
curl -s -o /dev/null -w '%{http_code}\n'        localhost:8710/healthz          # 200
curl -s -o /dev/null -w '%{http_code}\n'        localhost:8710/api/v1/status    # 401
curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer smoke-token-0123456789" \
                                                localhost:8710/api/v1/status    # 200
docker rm -f "$CID"
```

`make docker-smoke` is what CI runs (the `docker` job, push-only); the manual
sequence is for debugging it. If `/healthz` never answers, `docker logs` is the
first stop — a `DbLocked` message means the volume is still held by another
container.

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
