//! Durable jobs queue with retry, DLQ, and idempotency (issue 0026).
//!
//! Jobs make extraction async and auditable: `enqueue` is idempotent on
//! `idempotency_key`, `claim_next` leases one queued job to a worker,
//! `fail` retries with exponential backoff (`2^attempts` seconds) until
//! `max_attempts` (default 5) is exhausted — then the row moves to
//! `jobs_dead` with status `dead`. `requeue_dead` recovers it.

use rusqlite::OptionalExtension;

use crate::error::{Error, Result};
use crate::storage::StoreHandle;
use crate::util::{Clock, SystemClock, sha256_hex};

/// Default attempts before a job goes to the DLQ.
pub const DEFAULT_MAX_ATTEMPTS: i64 = 5;

/// Poll interval for the worker loop.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// One job row.
#[derive(Debug, Clone)]
pub struct JobRow {
    /// Internal id.
    pub id: i64,
    /// extract / summarize / maintain / reembed / reeval.
    pub kind: String,
    /// JSON payload for the handler.
    pub payload_json: String,
    /// Attempts used so far.
    pub attempts: i64,
}

/// Valid job kinds (matches the schema CHECK).
const KINDS: &[&str] = &["extract", "summarize", "maintain", "reembed", "reeval"];

fn check_kind(kind: &str) -> Result<()> {
    if !KINDS.contains(&kind) {
        return Err(Error::InvalidInput(format!(
            "job kind must be one of extract/summarize/maintain/reembed/reeval, got '{kind}'"
        )));
    }
    Ok(())
}

/// Enqueue a job; same `idempotency_key` returns the existing id.
/// When `key` is `None`, one is derived from kind + payload.
pub async fn enqueue(
    store: &StoreHandle,
    kind: &str,
    payload_json: &str,
    key: Option<&str>,
) -> Result<i64> {
    check_kind(kind)?;
    let key = key
        .map(str::to_string)
        .unwrap_or_else(|| sha256_hex(&format!("{kind}:{payload_json}")));
    let kind = kind.to_string();
    let payload = payload_json.to_string();
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            if let Some(id) = conn
                .query_row(
                    "SELECT id FROM jobs WHERE idempotency_key = ?1",
                    [&key],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            {
                return Ok(id);
            }
            conn.execute(
                "INSERT INTO jobs (kind, payload_json, status, max_attempts, run_after,
                                   idempotency_key, created_at, updated_at)
                 VALUES (?1, ?2, 'queued', ?3, 0, ?4, ?5, ?5)",
                rusqlite::params![&kind, &payload, DEFAULT_MAX_ATTEMPTS, &key, now],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await
}
/// Claim the oldest queued job whose `run_after` has passed.
/// Marks it running, bumps attempts, returns it. `None` when idle.
pub async fn claim_next(store: &StoreHandle, owner: &str) -> Result<Option<JobRow>> {
    let owner = owner.to_string();
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            let row: Option<(i64, String, String, i64)> = conn
                .query_row(
                    "SELECT id, kind, payload_json, attempts FROM jobs
                     WHERE status = 'queued' AND run_after <= ?1
                     ORDER BY id LIMIT 1",
                    [now],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            match row {
                None => Ok(None),
                Some((id, kind, payload, attempts)) => {
                    conn.execute(
                        "UPDATE jobs SET status = 'running', attempts = attempts + 1,
                                        locked_at = ?1, lock_owner = ?2, updated_at = ?1
                         WHERE id = ?3",
                        rusqlite::params![now, &owner, id],
                    )?;
                    Ok(Some(JobRow {
                        id,
                        kind,
                        payload_json: payload,
                        attempts: attempts + 1,
                    }))
                }
            }
        })
        .await
}

/// Mark a job done.
pub async fn complete(store: &StoreHandle, job_id: i64) -> Result<()> {
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET status = 'done', locked_at = NULL, lock_owner = NULL,
                                 updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now, job_id],
            )?;
            Ok(())
        })
        .await
}

/// Fail a job: retry with backoff, or move to the DLQ when attempts exhausted.
pub async fn fail(store: &StoreHandle, job_id: i64, err: &str) -> Result<()> {
    let err = err.to_string();
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            let (attempts, max, kind, payload): (i64, i64, String, String) = conn.query_row(
                "SELECT attempts, max_attempts, kind, payload_json FROM jobs WHERE id = ?1",
                [job_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            if attempts < max {
                let backoff_secs = 1i64 << attempts.min(10);
                conn.execute(
                    "UPDATE jobs SET status = 'queued', run_after = ?1, last_error = ?2,
                                     locked_at = NULL, lock_owner = NULL, updated_at = ?3
                     WHERE id = ?4",
                    rusqlite::params![now + backoff_secs * 1000, &err, now, job_id],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO jobs_dead (job_id, kind, payload_json, error, attempts, failed_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![job_id, &kind, &payload, &err, attempts, now],
                )?;
                conn.execute(
                    "UPDATE jobs SET status = 'dead', last_error = ?1,
                                     locked_at = NULL, lock_owner = NULL, updated_at = ?2
                     WHERE id = ?3",
                    rusqlite::params![&err, now, job_id],
                )?;
            }
            Ok(())
        })
        .await
}

/// Recover a dead job back to queued (clears attempts).
pub async fn requeue_dead(store: &StoreHandle, job_id: i64) -> Result<()> {
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            let n = conn.execute(
                "UPDATE jobs SET status = 'queued', attempts = 0, run_after = 0,
                                 last_error = NULL, updated_at = ?1
                 WHERE id = ?2 AND status = 'dead'",
                rusqlite::params![now, job_id],
            )?;
            if n == 0 {
                return Err(Error::InvalidInput(format!(
                    "job {job_id} is not dead; nothing to requeue"
                )));
            }
            conn.execute(
                "UPDATE jobs_dead SET reprocessed_at = ?1
                 WHERE job_id = ?2 AND reprocessed_at IS NULL",
                rusqlite::params![now, job_id],
            )?;
            Ok(())
        })
        .await
}

/// Count rows in `jobs_dead` (for tests and the doctor command).
pub async fn dead_count(store: &StoreHandle) -> Result<i64> {
    store
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM jobs_dead", [], |r| r.get(0))
                .map_err(|e| e.into())
        })
        .await
}
