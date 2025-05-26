//! Write path: sessions, turns, jobs, extraction, persistence (v0.2.0).
//!
//! The write path turns conversation turns into durable memories:
//!
//! - [`sessions`] — open/append/close with idle timeout
//! - jobs/worker, extractor, persist/dedup, trust/redact — land in 0026..0029
//! - [`remember`] — the public entry: one fact in, one memory out

pub mod sessions;

pub use sessions::{SessionRow, TurnRow};
