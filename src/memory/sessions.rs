//! Sessions & turns API (issue 0025).
//!
//! Thin async layer over the `sessions`/`turns` tables from schema v1.
//! Turns are the raw conversation log — persisted verbatim (redaction happens
//! at extraction time in 0029, not here). Concurrency is serialized by the
//! single-writer actor: `seq` is `max+1` inside one writer closure.

use rusqlite::OptionalExtension;

use crate::error::{Error, Result};
use crate::storage::StoreHandle;
use crate::util::{Clock, SystemClock, TokenCounter, sha256_hex};

/// One session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Internal id.
    pub id: i64,
    /// External stable id.
    pub public_id: String,
    /// Owning agent.
    pub agent_id: String,
    /// `open` / `closed`.
    pub status: String,
}

/// One turn row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnRow {
    /// Internal id.
    pub id: i64,
    /// Owning session.
    pub session_id: i64,
    /// 1-based sequence within the session.
    pub seq: i64,
    /// user / assistant / system / tool.
    pub role: String,
    /// Raw content.
    pub content: String,
}

/// Valid turn roles (matches the schema CHECK).
const ROLES: &[&str] = &["user", "assistant", "system", "tool"];

/// Open a session for `agent_id`; returns the row.
pub async fn open_session(store: &StoreHandle, agent_id: &str) -> Result<SessionRow> {
    if agent_id.trim().is_empty() {
        return Err(Error::InvalidInput("agent_id must not be empty".into()));
    }
    let public_id = uuid::Uuid::new_v4().to_string();
    let agent = agent_id.to_string();
    let pid = public_id.clone();
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO sessions (public_id, agent_id, started_at, status)
                 VALUES (?1, ?2, ?3, 'open')",
                rusqlite::params![&pid, &agent, now],
            )?;
            Ok(SessionRow {
                id: conn.last_insert_rowid(),
                public_id: pid,
                agent_id: agent,
                status: "open".into(),
            })
        })
        .await
}
/// Append a turn; `seq` is assigned as `max+1` inside the writer closure.
pub async fn append_turn(
    store: &StoreHandle,
    session_id: i64,
    role: &str,
    content: &str,
) -> Result<TurnRow> {
    if !ROLES.contains(&role) {
        return Err(Error::InvalidInput(format!(
            "role must be one of user/assistant/system/tool, got '{role}'"
        )));
    }
    if content.is_empty() {
        return Err(Error::InvalidInput("turn content must not be empty".into()));
    }
    let role = role.to_string();
    let content = content.to_string();
    let hash = sha256_hex(&content);
    let counter = crate::util::HeuristicCounter::new();
    let tokens = counter.count(&content) as i64;
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            let open: Option<i64> = conn
                .query_row(
                    "SELECT id FROM sessions WHERE id = ?1 AND status = 'open'",
                    [session_id],
                    |r| r.get(0),
                )
                .optional()?;
            if open.is_none() {
                return Err(Error::InvalidInput(format!(
                    "session {session_id} does not exist or is closed"
                )));
            }
            let seq: i64 = conn.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM turns WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )?;
            conn.execute(
                "INSERT INTO turns (session_id, seq, role, content, content_hash, created_at, tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![session_id, seq, &role, &content, &hash, now, tokens],
            )?;
            Ok(TurnRow {
                id: conn.last_insert_rowid(),
                session_id,
                seq,
                role,
                content,
            })
        })
        .await
}

/// Close a session with `reason` (explicit / idle / forced).
pub async fn close_session(store: &StoreHandle, session_id: i64, reason: &str) -> Result<()> {
    if !["explicit", "idle", "forced"].contains(&reason) {
        return Err(Error::InvalidInput(format!(
            "end_reason must be explicit/idle/forced, got '{reason}'"
        )));
    }
    let reason = reason.to_string();
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            let n = conn.execute(
                "UPDATE sessions SET status = 'closed', ended_at = ?1, end_reason = ?2
                 WHERE id = ?3 AND status = 'open'",
                rusqlite::params![now, &reason, session_id],
            )?;
            if n == 0 {
                return Err(Error::InvalidInput(format!(
                    "session {session_id} does not exist or is already closed"
                )));
            }
            Ok(())
        })
        .await
}

/// Close sessions idle longer than `idle_minutes`. Returns count closed.
/// Close sessions idle longer than `idle_minutes`; returns the ids closed.
///
/// Returning the ids (not just a count) lets the caller enqueue per-session
/// work — e.g. extraction — for every session that just closed (0055).
pub async fn close_idle_sessions(store: &StoreHandle, idle_minutes: u64) -> Result<Vec<i64>> {
    let cutoff = SystemClock.now_millis() - (idle_minutes as i64) * 60_000;
    let now = SystemClock.now_millis();
    store
        .write(move |conn| {
            conn.execute(
                "UPDATE sessions SET status = 'closed', ended_at = ?1, end_reason = 'idle'
                 WHERE status = 'open'
                   AND COALESCE(
                         (SELECT MAX(created_at) FROM turns WHERE turns.session_id = sessions.id),
                         started_at
                       ) < ?2",
                rusqlite::params![now, cutoff],
            )?;
            let mut stmt = conn.prepare(
                "SELECT id FROM sessions WHERE status = 'closed' AND end_reason = 'idle'
                  AND ended_at = ?1",
            )?;
            let rows: Vec<i64> = stmt
                .query_map(rusqlite::params![now], |r| r.get::<_, i64>(0))?
                .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
                .map_err(|e| crate::error::Error::Storage(e.to_string()))?;
            Ok(rows)
        })
        .await
}
/// Latest open session for an agent, if any.
pub async fn get_open_session(store: &StoreHandle, agent_id: &str) -> Result<Option<SessionRow>> {
    let agent = agent_id.to_string();
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT id, public_id, agent_id, status FROM sessions
                 WHERE agent_id = ?1 AND status = 'open' ORDER BY started_at DESC LIMIT 1",
                [&agent],
                |r| {
                    Ok(SessionRow {
                        id: r.get(0)?,
                        public_id: r.get(1)?,
                        agent_id: r.get(2)?,
                        status: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| e.into())
        })
        .await
}
/// Internal id for a session's public id, if it exists.
pub async fn session_id_by_public(store: &StoreHandle, public_id: &str) -> Result<Option<i64>> {
    let public_id = public_id.to_string();
    store
        .read(move |conn| {
            conn.query_row(
                "SELECT id FROM sessions WHERE public_id = ?1",
                [&public_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.into())
        })
        .await
}

/// All turns of a session in seq order.
pub async fn session_turns(store: &StoreHandle, session_id: i64) -> Result<Vec<TurnRow>> {
    store
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, session_id, seq, role, content FROM turns
                 WHERE session_id = ?1 ORDER BY seq",
            )?;
            let rows = stmt
                .query_map([session_id], |r| {
                    Ok(TurnRow {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        seq: r.get(2)?,
                        role: r.get(3)?,
                        content: r.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    async fn test_store() -> (StoreHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            db_path: dir.path().join("t.db"),
            ..Config::default()
        };
        let store = StoreHandle::open(&cfg, 1).await.unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn session_open_append_close() {
        let (store, _dir) = test_store().await;
        let s = open_session(&store, "a").await.unwrap();
        assert_eq!(s.status, "open");
        let t1 = append_turn(&store, s.id, "user", "hello").await.unwrap();
        let t2 = append_turn(&store, s.id, "assistant", "hi").await.unwrap();
        assert_eq!((t1.seq, t2.seq), (1, 2));
        let turns = session_turns(&store, s.id).await.unwrap();
        assert_eq!(turns.len(), 2);
        close_session(&store, s.id, "explicit").await.unwrap();
        assert!(get_open_session(&store, "a").await.unwrap().is_none());
        let err = append_turn(&store, s.id, "user", "late").await.unwrap_err();
        assert!(err.to_string().contains("closed"), "got: {err}");
    }

    #[tokio::test]
    async fn idle_close_only_stale() {
        let (store, _dir) = test_store().await;
        let fresh = open_session(&store, "a").await.unwrap();
        append_turn(&store, fresh.id, "user", "now").await.unwrap();
        let stale = open_session(&store, "a").await.unwrap();
        store
            .write(move |conn| {
                conn.execute(
                    "UPDATE sessions SET started_at = ?1 WHERE id = ?2",
                    rusqlite::params![0i64, stale.id],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let closed = close_idle_sessions(&store, 30).await.unwrap();
        assert_eq!(closed.len(), 1);
        let still_open = get_open_session(&store, "a").await.unwrap().unwrap();
        assert_eq!(still_open.id, fresh.id);
    }

    #[tokio::test]
    async fn rejects_bad_role_and_reason() {
        let (store, _dir) = test_store().await;
        let s = open_session(&store, "a").await.unwrap();
        assert!(append_turn(&store, s.id, "bot", "x").await.is_err());
        assert!(close_session(&store, s.id, "whenever").await.is_err());
    }
}
