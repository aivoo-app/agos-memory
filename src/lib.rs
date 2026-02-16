//! # agos-memory
//!
//! Self-hosted agent memory manager: durable, tiered, citation-backed memory
//! for AI agents (Hermes, OpenClaw, or any MCP/HTTP client).
//!
//! Module map:
//! - [`config`] — layered configuration (defaults <- TOML <- env <- flags)
//! - [`error`] — typed error taxonomy with actionable messages
//! - [`storage`] — SQLite persistence: single-writer actor + read pool,
//!   migrations, sqlite-vec registration
//! - [`embed`] — embedding providers (openai_compat / hash-mock / degraded none)
//! - [`llm`] — chat clients for extraction (mock for now, real in v0.2.0)
//! - [`observe`] — tracing init + LLM call cost ledger
//! - [`eval`] — offline eval harness (precision / recall / MRR / leak count)
//! - [`util`] — clock, token counter, hashing
//!
//! The write path (extraction pipeline, sessions) is v0.2.0; the recall path
//! (hybrid FTS5 + vector retrieval) is v0.3.0; consolidation & forgetting
//! (summaries, versioning, verified deletion, TTL) is v0.4.0. See
//! `docs/architecture.md` and `docs/forget.md`.

pub mod cli;
pub mod config;
pub mod embed;
pub mod error;
pub mod eval;
pub mod http;
pub mod llm;
pub mod memory;
pub mod observe;
pub mod recall;
pub mod sqlite_vec;
pub mod storage;
pub mod util;

/// Crate version from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Name used in logs, locks, and MCP server info.
pub const NAME: &str = "agos-memory";

/// Storage defaults exposed for CLI and docs.
pub mod defaults {
    /// Read connections in the pool.
    pub const READ_POOL_SIZE: usize = 4;
}
