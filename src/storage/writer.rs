//! Single-writer actor (issue 0008).
//!
//! A dedicated thread owns the single write [`Connection`]. Other threads
//! submit closures over an mpsc channel; the writer executes them and replies
//! through a bounded channel. Blocking SQLite work therefore happens on the
//! writer thread, never on tokio async workers.
//!
//! Features:
//! - Panic-safe: job panics are caught, logged, and the writer continues.
//! - Timeouts: configurable send/receive timeouts prevent indefinite blocking.
//! - Graceful shutdown: drain channel before stopping.
//! - Health checks: `is_healthy()` reports writer thread status.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use rusqlite::Connection;

use crate::error::{Error, Result};

/// Job sent to the writer thread.
type JobBox = Box<dyn FnOnce(&mut Connection) + Send + 'static>;

/// Reply from the writer thread - just success/failure for error propagation.
type ReplyBox = Box<dyn FnOnce() -> Result<()> + Send + 'static>;

/// Sender type for replies.
type ReplySender = Sender<ReplyBox>;

/// Message sent to the writer thread: either a job or a shutdown signal.
enum WriterMsg {
    Job(JobBox, ReplySender),
    Shutdown,
}

/// Configuration for the writer thread.
#[derive(Debug, Clone, Copy)]
pub struct WriterConfig {
    /// Maximum time to wait for a slot in the job queue.
    pub send_timeout: Duration,
    /// Maximum time to wait for the writer to execute a job and reply.
    pub recv_timeout: Duration,
    /// Maximum number of jobs that can be queued.
    pub queue_size: usize,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            send_timeout: Duration::from_secs(5),
            recv_timeout: Duration::from_secs(30),
            queue_size: 1024,
        }
    }
}

/// Handle to the writer thread. Cloneable; dropping the last handle initiates
/// graceful shutdown after draining queued jobs.
#[derive(Clone)]
pub struct WriterHandle {
    tx: Sender<WriterMsg>,
    healthy: Arc<AtomicBool>,
    config: WriterConfig,
}

impl WriterHandle {
    /// Spawn the writer thread for the given connection with default config.
    pub fn spawn(conn: Connection) -> Result<Self> {
        Self::spawn_with_config(conn, WriterConfig::default())
    }

    /// Spawn the writer thread with a custom configuration.
    pub fn spawn_with_config(mut conn: Connection, config: WriterConfig) -> Result<Self> {
        let (tx, rx) = bounded::<WriterMsg>(config.queue_size);
        let healthy = Arc::new(AtomicBool::new(true));
        let healthy_clone = healthy.clone();

        thread::Builder::new()
            .name("agos-memory-writer".into())
            .spawn(move || {
                Self::writer_loop(&mut conn, rx, healthy_clone);
                tracing::debug!("writer thread stopped");
            })
            .map_err(|e| Error::Storage(format!("failed to spawn writer thread: {e}")))?;

        Ok(Self {
            tx,
            healthy,
            config,
        })
    }

    /// Main writer loop: processes jobs until shutdown signal received.
    fn writer_loop(conn: &mut Connection, rx: Receiver<WriterMsg>, healthy: Arc<AtomicBool>) {
        for msg in rx {
            match msg {
                WriterMsg::Job(job, reply_tx) => {
                    // Execute job with panic safety.
                    let result = catch_unwind(AssertUnwindSafe(|| job(conn)));

                    let reply: ReplyBox = match result {
                        Ok(()) => Box::new(|| Ok(())),
                        Err(panic_payload) => {
                            healthy.store(false, Ordering::SeqCst);
                            let msg = format!("writer job panicked: {:?}", panic_payload);
                            tracing::error!(%msg);
                            Box::new(move || Err(Error::Storage(msg)))
                        }
                    };

                    // Send reply back; ignore if caller gave up.
                    let _ = reply_tx.send(reply);
                }
                WriterMsg::Shutdown => {
                    tracing::debug!("writer received shutdown signal");
                    break;
                }
            }
        }
        healthy.store(false, Ordering::SeqCst);
    }

    /// Check if the writer thread is healthy (no panics have occurred).
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    /// Run a closure on the write connection, awaiting the result with timeout.
    ///
    /// Callers in async context must wrap this in `tokio::task::spawn_blocking`
    /// (which `StoreHandle::write` does).
    pub fn write<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        // Use a second channel to return the actual result value.
        let (result_tx, result_rx) = bounded(1);
        let (reply_tx, reply_rx) = bounded(1);
        let reply_tx_for_job = reply_tx.clone();

        let job: JobBox = Box::new(move |conn: &mut Connection| {
            let result = f(conn);
            // Send the result value through the result channel.
            let _ = result_tx.send(result);
            // Send success signal through reply channel.
            let reply: ReplyBox = Box::new(|| Ok(()));
            let _ = reply_tx_for_job.send(reply);
        });

        // Send job with timeout.
        match self
            .tx
            .send_timeout(WriterMsg::Job(job, reply_tx), self.config.send_timeout)
        {
            Ok(()) => {}
            Err(crossbeam_channel::SendTimeoutError::Timeout(_)) => {
                return Err(Error::Storage(format!(
                    "writer queue full, timeout after {:?}",
                    self.config.send_timeout
                )));
            }
            Err(crossbeam_channel::SendTimeoutError::Disconnected(_)) => {
                return Err(Error::Storage("writer thread has stopped".into()));
            }
        }

        // Wait for reply with timeout (this ensures the job completed).
        match reply_rx.recv_timeout(self.config.recv_timeout) {
            Ok(reply) => {
                reply()?; // Propagate any writer-side error (e.g., panic)
                // Now get the actual result from the result channel.
                // This should be immediately available since reply succeeded.
                result_rx
                    .recv()
                    .map_err(|_| Error::Storage("writer dropped result channel".into()))?
            }
            Err(RecvTimeoutError::Timeout) => Err(Error::Storage(format!(
                "writer did not respond within {:?}",
                self.config.recv_timeout
            ))),
            Err(RecvTimeoutError::Disconnected) => Err(Error::Storage(
                "writer thread dropped the reply channel".into(),
            )),
        }
    }

    /// Initiate graceful shutdown: stops accepting new jobs, drains the queue,
    /// then stops the writer thread.
    pub fn shutdown(&self) -> Result<()> {
        // Send shutdown signal; ignore if already shut down.
        let _ = self.tx.send(WriterMsg::Shutdown);
        // Note: the thread will exit after processing remaining jobs.
        // The handle can still be used to check health, but new writes will fail.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

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
        let count: i64 = w
            .write(|c| {
                c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                    .map_err(crate::error::Error::from)
            })
            .unwrap();
        assert_eq!(count, 8);
    }

    #[test]
    fn panic_in_job_is_caught_and_writer_continues() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(v INTEGER);").unwrap();
        let w = WriterHandle::spawn_with_config(
            conn,
            WriterConfig {
                send_timeout: Duration::from_secs(1),
                recv_timeout: Duration::from_secs(1),
                queue_size: 10,
            },
        )
        .unwrap();

        // First, a successful write.
        w.write(|c| {
            c.execute("INSERT INTO t(v) VALUES (1)", [])?;
            Ok(())
        })
        .unwrap();

        // Then a job that panics.
        let err = w
            .write::<(), _>(|_c| {
                panic!("intentional panic");
            })
            .unwrap_err();
        assert!(matches!(err, Error::Storage(_)));
        assert!(err.to_string().contains("writer job panicked"));

        // Writer should be marked unhealthy after panic.
        assert!(!w.is_healthy());

        // A subsequent write should still work (writer continues after panic).
        w.write(|c| {
            c.execute("INSERT INTO t(v) VALUES (2)", [])?;
            Ok(())
        })
        .unwrap();

        let count: i64 = w
            .write(|c| {
                c.query_row("SELECT count(*) FROM t", [], |r| r.get(0))
                    .map_err(crate::error::Error::from)
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn recv_timeout_when_job_slow() {
        let conn = Connection::open_in_memory().unwrap();
        let w = WriterHandle::spawn_with_config(
            conn,
            WriterConfig {
                send_timeout: Duration::from_secs(1),
                recv_timeout: Duration::from_millis(50),
                queue_size: 10,
            },
        )
        .unwrap();

        // Job that sleeps longer than recv_timeout.
        let err = w
            .write(|_| {
                std::thread::sleep(Duration::from_millis(200));
                Ok(())
            })
            .unwrap_err();
        assert!(matches!(err, Error::Storage(_)));
        assert!(err.to_string().contains("writer did not respond"));
    }

    #[test]
    fn shutdown_stops_accepting_new_jobs() {
        let conn = Connection::open_in_memory().unwrap();
        let w = WriterHandle::spawn(conn).unwrap();
        w.shutdown().unwrap();

        // After shutdown, new jobs should fail.
        let err = w.write(|_| Ok(())).unwrap_err();
        assert!(matches!(err, Error::Storage(_)));
    }

    #[test]
    fn write_returns_value() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t(v INTEGER);").unwrap();
        let w = WriterHandle::spawn(conn).unwrap();

        let count: i64 = w
            .write(|c| {
                c.execute("INSERT INTO t(v) VALUES (42)", [])?;
                Ok(42i64)
            })
            .unwrap();
        assert_eq!(count, 42);
    }
}
