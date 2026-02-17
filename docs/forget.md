# Forgetting: deletion, TTL retention, audit (v0.4.0)

Memory management has a delete side. agos-memory never loses a fact silently:
every removal is two-phase, audited, and — for hard deletes — provable by
replay (tombstone + row counts + VACUUM timing). See
[ADR-008](adr/008-verified-deletion-and-consolidation.md) for the rationale.

## Two-phase deletion

```
agos-memory forget soft <public_id> [--reason <why>]     # deprecate (recoverable)
agos-memory forget restore <public_id>                   # back to active
agos-memory forget hard <public_id> [--reason <why>]     # verified purge + tombstone
agos-memory forget rollback <public_id> --to-version <n> # version rollback (new head)
```

- **Soft deprecate** sets `status='deprecated'`, stamps `deleted_at`, and the
  hard filter drops the row from every recall path. Restore flips it back.
- **Hard purge** deletes, in one writer transaction: `memory_links` (both
  directions), `memory_versions`, `recall_items`, `pins`, `vec_memories`, then
  the `memories` row (FTS row goes via trigger). After the deletes, a
  per-table `COUNT(*)` must be **zero** — any survivor aborts the purge with
  the table named (fail-closed). Post-commit: `VACUUM` + `wal_checkpoint`,
  then a `tombstones` row with `rowcount_before`, `rowcount_after`,
  `vacuum_duration_ms`.

## Inspection surfaces

```
agos-memory forget list-tombstones                       # hard-purged memories
agos-memory forget list-audit [--action <a>] [--since <epoch-ms>] [--limit <n>]
```

Actions: `soft_deprecate`, `hard_purge`, `restore`, `ttl_deprecate`,
`ttl_purge`. Both `tombstones` and `forget_audit` are append-only, enforced by
schema-v4 triggers (`BEFORE UPDATE/DELETE → RAISE(ABORT)`), so the ledger
cannot be rewritten, even by this binary.

## TTL retention

Per-tier TTLs live under `[memory]`:

```toml
[memory]
ttl_working_days = 30
ttl_episodic_days = 90
ttl_semantic_days = 0      # 0 = never
ttl_procedural_days = 0    # 0 = never
ttl_grace_days = 30        # deprecated rows are purged only after this
reaper_hour = 3            # UTC hour the scheduler enqueues the reaper
```

The reaper (via the `maintain` job kind, payload `{"action":"ttl"}`):

1. Finds active memories past their per-tier TTL → soft-deprecate, audited as
   `ttl_deprecate` (requester `ttl-reaper`).
2. Finds deprecated memories older than `ttl_grace_days` → verified hard
   purge, audited as `ttl_purge`.

Run it on demand with `maintain --ttl`, or leave it to the scheduler
(`maintain --schedule`), which enqueues `maintain` jobs at `reaper_hour` and
consolidation at `[consolidate] day/hour`; `enqueue`'s idempotency keys make
re-ticking a no-op. Throughput contract: the reaper processes ≥ 1000
memories/s (release profile; `tests/consolidate_bench.rs`).

## Consolidation

`maintain --consolidate` (or the scheduler's weekly slot) runs, idempotently:

1. **Summarization pass** — memories older than `[memory]
   summarize_after_days_<tier>` without a summary get one via the LLM.
2. **Dedup** — cosine over **stored** `vec_memories` embeddings within a tier
   (text-proxy fallback for vectorless rows); above `dedup_threshold` the
   cluster owner's `ref_count` is bumped and the newcomer tagged.
3. **Orphan cleanup** — `memory_versions` rows no longer referenced by any
   memory are deleted.

Quality: `make eval-summarize` gates mean ROUGE-L ≥ 0.85 over 100 hermetic
fixtures; `make bench-consolidate` reports summarize speed, batch throughput,
dedup and reaper rates in release profile.
