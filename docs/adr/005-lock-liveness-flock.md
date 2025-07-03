# ADR-005: Process-lock liveness via `flock`, not pid files

Status: accepted, 2026-09-20

## Context

ADR-003 enforces one process per database with a sidecar lock file. The v0.1.0
implementation wrote a pid into `<db>.lock` and treated "the file exists" as
"someone holds the lock". That is wrong in two ways that both showed up in
practice:

1. A process killed with `SIGKILL` (or a host reboot) leaves the file behind.
   The next `agos-memory` run then refuses to open its own database until the
   operator deletes the file by hand — a self-inflicted outage.
2. Liveness could not be probed reliably in-process, so the `DbLocked` error
   reported a constant `pid 0`, which tells the operator nothing about who to
   stop.

## Decision

- The lock is an exclusive, **non-blocking `flock(LOCK_EX | LOCK_NB)`** on the
  `<db>.lock` sidecar, held for as long as the `Store` lives.
- The pid is still written into the sidecar, but only as *diagnostic
  metadata*. Liveness comes from the kernel: when the holder dies, the kernel
  drops the lock, so the next open succeeds and rewrites the sidecar.
- A failed acquisition reads the recorded pid back and reports it:

  ```
  database /x/y.db is locked by another agos-memory process (pid 1234);
  one process per database is enforced — stop it or use a different --db
  ```

- Trying to acquire the lock twice in the *same* process also fails, so two
  `StoreHandle`s on one database in one process cannot silently coexist.

## Consequences

- Stale locks self-heal; there is no manual cleanup step in the runbook.
- `DbLocked` is actionable and names a real pid.
- Windows support: `flock` is a Unix API. The lock module is isolated so a
  Windows path can be added behind a `cfg(windows)` implementation
  (`LockFileEx`) without touching callers; until then Windows is not a target.
- Regression coverage: `tests/store_restart.rs` asserts a store reopens after
  a normal drop, and the lock error carries the recorded pid.
