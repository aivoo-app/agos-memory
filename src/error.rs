//! Error taxonomy for `agos_memory`.
//!
//! Every fallible API returns [`Error`]; the CLI boundary wraps it in
//! `anyhow` for context and maps variants to exit codes (see `main.rs`).
//!
//! Invariant: `Display` messages must be actionable — they tell the user
//! what to do next, not just what broke.

use thiserror::Error;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// All recoverable failures surfaced by agos-memory.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// Configuration failed validation. Fix the named key.
    #[error("configuration error: {0}")]
    Config(String),

    /// The SQLite database could not be opened or prepared.
    #[error("storage error: {0}")]
    Storage(String),

    /// Database schema is newer than this binary; upgrade required.
    #[error(
        "database schema version {db} is newer than supported version {supported}; \
         upgrade agos-memory before opening this database"
    )]
    SchemaTooNew {
        /// Schema version found on disk.
        db: i64,
        /// Highest schema version this binary supports.
        supported: i64,
    },

    /// Another agos-memory process holds this database.
    #[error(
        "database {path} is locked by another agos-memory process (pid {pid}); \
         one process per database is enforced — stop it or use a different --db"
    )]
    DbLocked {
        /// Database file path.
        path: String,
        /// Holding process id recorded in the lock file.
        pid: u32,
    },

    /// The embedding provider configuration is invalid or unreachable.
    #[error("embedding provider error: {0}")]
    Embedder(String),

    /// Embedding model or dimension mismatch with what the database was built with.
    #[error(
        "embedding mismatch: database expects model '{model}' with dim {dim}, \
         got model '{found}' with dim {found_dim}; run `agos-memory reembed` to rebuild"
    )]
    EmbeddingMismatch {
        /// Model recorded in the database.
        model: String,
        /// Dimension recorded in the database.
        dim: i64,
        /// Model the provider produced.
        found: String,
        /// Dimension the provider produced.
        found_dim: i64,
    },

    /// An LLM call failed or returned an unusable response.
    #[error("llm error: {0}")]
    Llm(String),

    /// The per-session token cost ceiling was breached; recall degrades to keyword-only.
    #[error(
        "session token ceiling exceeded ({used} > {ceiling}); degrading to keyword-only recall"
    )]
    BudgetExceeded {
        /// Tokens used so far this session.
        used: u64,
        /// Configured ceiling.
        ceiling: u64,
    },

    /// Input failed validation.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Storage(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_actionable() {
        let e = Error::SchemaTooNew {
            db: 3,
            supported: 1,
        };
        let msg = e.to_string();
        assert!(msg.contains("upgrade agos-memory"), "got: {msg}");

        let e = Error::DbLocked {
            path: "/tmp/a.db".into(),
            pid: 42,
        };
        let msg = e.to_string();
        assert!(msg.contains("one process per database"), "got: {msg}");

        let e = Error::EmbeddingMismatch {
            model: "m1".into(),
            dim: 384,
            found: "m2".into(),
            found_dim: 768,
        };
        let msg = e.to_string();
        assert!(msg.contains("reembed"), "got: {msg}");
    }

    #[test]
    fn from_rusqlite_wraps() {
        let e: Error = rusqlite::Error::InvalidColumnName("x".into()).into();
        assert!(matches!(e, Error::Storage(_)));
    }
}
