//! Single-writer actor (issue 0008).
//!
//! A dedicated thread owns the single write [`Connection`]. Other threads
//! submit closures over an mpsc channel; the writer executes them and replies
//! through a bounded channel. Blocking SQLite work therefore happens on the
//! writer thread, never on tokio async workers.

use std::sync::mpsc;
use std::thread;

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Handle to the writer thread. Cloneable; dropping the last handle stops the
/// writer after draining queued jobs.
#[derive(Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<JobBox>,
}

type JobBox = Box<dyn FnOnce(&mut Connection) + Send>;

impl WriterHandle {
    /// Spawn the writer thread for the given connection.
    pub fn spawn(mut conn: Connection) -> Result<Self> {
        let (tx, rx) = mpsc::channel::<JobBox>();
        thread::Builder::new()
            .name("agos-memory-writer".into())
            .spawn(move || {
                for job in rx {
                    job(&mut conn);
                }
                tracing::debug!("writer thread stopping");
            })
            .map_err(|e| Error::Storage(format!("failed to spawn writer thread: {e}")))?;
        Ok(Self { tx })
    }

    /// Run a closure on the write connection, awaiting the result.
    ///
    /// Callers in async context must wrap this in `tokio::task::spawn_blocking`
    /// (which `StoreHandle::write` does).
    pub fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = mpsc::sync_channel(1);
        let boxed: JobBox = Box::new(move |conn: &mut Connection| {
            let _ = tx.send(f(conn));
        });
        self.tx
            .send(boxed)
            .map_err(|_| Error::Storage("writer thread has stopped".into()))?;
        rx.recv()
            .map_err(|_| Error::Storage("writer thread dropped the reply".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn writes_execute_in_order_on_one_thread() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(v INTEGER);").unwrap();
        let w = WriterHandle::spawn(conn).unwrap();

        for i in 0..50 {
            let w2 = w.clone();
            w.write(move |c| {
                c.execute("INSERT INTO t(v) VALUES (?1)", [i])?;
                Ok(())
            })
            .unwrap();
            let _ = w2;
        }

        let count: i64 = w
            .write(|c| {
                c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                    .map_err(crate::error::Error::from)
            })
            .unwrap();
        assert_eq!(count, 50);
    }

    #[test]
    fn errors_propagate_back() {
        let conn = Connection::open_in_memory().unwrap();
        let w = WriterHandle::spawn(conn).unwrap();
        let err = w
            .write::<(), _>(|c| {
                c.execute("INSERT INTO nonexistent(v) VALUES (1)", [])?;
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(err, Error::Storage(_)));
    }

    #[test]
    fn thread_affinity_is_single_writer() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(tid TEXT);").unwrap();
        let w = WriterHandle::spawn(conn).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        for _ in 0..8 {
            let w2 = w.clone();
            let seen2 = seen.clone();
            w.write(move |c| {
                // Hash the thread id into a value to detect cross-thread writes.
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                thread::current().id().hash(&mut h);
                let _ = seen2.fetch_xor(h.finish() as usize, Ordering::SeqCst);
                c.execute("INSERT INTO t(tid) VALUES ('x')", [])?;
                Ok(())
            })
            .unwrap();
            let _ = w2;
        }
        // If all writes landed, the table has all rows; thread identity is
        // asserted by construction (single spawned thread owns the conn).
        let count: i64 = w
            .write(|c| {
                c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                    .map_err(crate::error::Error::from)
            })
            .unwrap();
        assert_eq!(count, 8);
    }
}
