//! Read-side connection pool (issue 0008).
//!
//! Hand-rolled to keep dependencies minimal (per CONTRIBUTING). Connections
//! are *checked out* (moved out of the pool) and returned on drop. rusqlite's
//! `Connection` is `Send` but not `Sync`, so checkout-by-move (rather than
//! shared `Arc` handles) is what makes pooled connections usable across
//! threads and inside `spawn_blocking`.

use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use rusqlite::{Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::storage::schema;

/// Inner pool: fixed set of slots, `None` while checked out.
struct Inner {
    slots: Mutex<Vec<Option<Connection>>>,
    /// Signaled when a slot is returned to the pool.
    not_empty: Condvar,
}

/// A read connection checked out of the pool; returned on drop.
pub struct PooledConn {
    inner: Arc<Inner>,
    slot: usize,
    conn: Option<Connection>,
}

impl Deref for PooledConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("connection present while PooledConn is alive")
    }
}

impl DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn
            .as_mut()
            .expect("connection present while PooledConn is alive")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Never panic in Drop: if the pool mutex is poisoned, just drop
            // the connection rather than propagating the panic.
            if let Ok(mut slots) = self.inner.slots.lock() {
                slots[self.slot] = Some(conn);
                self.inner.not_empty.notify_one();
            }
        }
    }
}

/// A pool of read connections over one database file.
pub struct ReadPool {
    inner: Arc<Inner>,
    size: usize,
}

impl ReadPool {
    /// Open `size` read connections. `size == 0` is promoted to 1.
    pub fn open(path: &Path, size: usize) -> Result<Self> {
        let size = size.max(1);
        let mut slots = Vec::with_capacity(size);
        for _ in 0..size {
            let conn = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .map_err(|e| Error::Storage(format!("cannot open read connection: {e}")))?;
            schema::apply_pragmas_read(&conn)?;
            slots.push(Some(conn));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                slots: Mutex::new(slots),
                not_empty: Condvar::new(),
            }),
            size,
        })
    }

    /// Check out a read connection, blocking until one is available.
    pub fn get(&self) -> PooledConn {
        // Handle poisoned mutexes gracefully: a previous panic should not
        // permanently break the pool.
        let mut slots = self.inner.slots.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(slot) = slots.iter().position(|s| s.is_some()) {
                let conn = slots[slot].take().expect("checked above");
                return PooledConn {
                    inner: self.inner.clone(),
                    slot,
                    conn: Some(conn),
                };
            }
            slots = self
                .inner
                .not_empty
                .wait(slots)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Number of connections in the pool.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Number of connections currently available.
    pub fn available(&self) -> usize {
        self.inner
            .slots
            .lock()
            .map(|s| s.iter().filter(|s| s.is_some()).count())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch("CREATE TABLE t(v INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    #[test]
    fn checkout_and_return() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        setup(&path);
        let pool = ReadPool::open(&path, 2).unwrap();
        assert_eq!(pool.size(), 2);

        {
            let c = pool.get();
            assert_eq!(pool.available(), 1);
            let _: i64 = c
                .query_row("SELECT v FROM t LIMIT 1", [], |r| r.get(0))
                .unwrap();
        }
        assert_eq!(pool.available(), 2, "connection must return on drop");
    }

    #[test]
    fn conn_is_usable_in_spawn_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        setup(&path);
        let pool = Arc::new(ReadPool::open(&path, 2).unwrap());
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let p = pool.clone();
            let n = tokio::task::spawn_blocking(move || {
                let c = p.get();
                c.query_row("SELECT v FROM t LIMIT 1", [], |r| r.get::<_, i64>(0))
                    .unwrap()
            })
            .await
            .unwrap();
            assert_eq!(n, 1);
        });
    }

    #[test]
    fn blocks_until_connection_available() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        setup(&path);

        let pool = Arc::new(ReadPool::open(&path, 2).unwrap());
        let p2 = pool.clone();

        // Check out both connections so the pool is empty.
        let _c1 = pool.get();
        let _c2 = pool.get();
        assert_eq!(pool.available(), 0);

        // A checkout on another thread should block until a connection returns.
        let handle = std::thread::spawn(move || {
            let c = p2.get();
            let _: i64 = c
                .query_row("SELECT v FROM t LIMIT 1", [], |r| r.get(0))
                .unwrap();
            "done"
        });

        // Drop one connection to unblock the waiting thread.
        drop(_c1);

        assert_eq!(handle.join().unwrap(), "done");
        drop(_c2);
    }
}
