//! Storage: SQLite-backed persistence with a single-writer actor (issues 0005-0008).
//!
//! Concurrency model (decision D3, answers agos-proxy plan Issue 3):
//!
//! - **One writer**: a dedicated OS thread owns the only write connection.
//!   Callers send write closures over an mpsc channel; blocking SQLite work
//!   happens on the writer thread, never on tokio async workers.
//! - **Read pool**: N read-only connections handed out round-robin.
//! - SQLite in WAL mode allows concurrent readers with the single writer.

pub mod pool;
pub mod schema;
pub mod store;
pub mod vecext;
pub mod writer;

pub use pool::ReadPool;
pub use store::{MemoryRow, NewMemory, SnapshotReport, Store, StoreHandle};
pub use writer::WriterHandle;
