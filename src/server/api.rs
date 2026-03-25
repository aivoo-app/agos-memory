//! Shared JSON-API plumbing for the axum HTTP surface (issue 0002).
//!
//! The JSON routes live alongside 0008's `/mcp` on one axum router so there
//! is one bind, one auth layer, and one shutdown path (D38). `/healthz` is
//! the only route layered *outside* the bearer middleware (liveness only).
//!
//! Every fallible route returns the CLI error taxonomy as `{error, code}`
//! with a matching HTTP status, so operators see the same vocabulary over
//! the wire as on stderr.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};

use crate::error::Error;

/// `{error, code}` wire body — mirrors the CLI taxonomy (`main.rs` maps the
/// same variants to process exit codes).
#[derive(Debug, serde::Serialize)]
pub struct ApiError {
    /// Human-readable, actionable message (`Display` of [`Error`]).
    pub error: String,
    /// Stable machine-readable code (the variant name in SCREAMING_SNAKE).
    pub code: &'static str,
}

/// Build the `{error, code}` body for one [`Error`].
pub fn api_error(e: &Error) -> ApiError {
    ApiError {
        error: e.to_string(),
        code: error_code(e),
    }
}

/// Stable code per variant — the `#[non_exhaustive]` future arm falls into
/// the storage bucket, matching `main.rs`'s exit-code mapping.
fn error_code(e: &Error) -> &'static str {
    match e {
        Error::Config(_) => "CONFIG",
        Error::InvalidInput(_) => "INVALID_INPUT",
        Error::MemoryNotFound { .. } => "NOT_FOUND",
        Error::SchemaTooNew { .. } => "SCHEMA_TOO_NEW",
        Error::DbLocked { .. } => "DB_LOCKED",
        Error::Storage(_) => "STORAGE",
        Error::Embedder(_) => "EMBEDDER",
        Error::EmbeddingMismatch { .. } => "EMBEDDING_MISMATCH",
        Error::Llm(_) => "LLM",
        Error::BudgetExceeded { .. } => "BUDGET_EXCEEDED",
    }
}

/// HTTP status per variant — 4xx for caller/actionable errors, 5xx for
/// backend failures, mirroring the CLI exit-code buckets.
pub fn status_for(e: &Error) -> StatusCode {
    match e {
        Error::Config(_) | Error::InvalidInput(_) => StatusCode::BAD_REQUEST,
        Error::MemoryNotFound { .. } => StatusCode::NOT_FOUND,
        Error::SchemaTooNew { .. } => StatusCode::SERVICE_UNAVAILABLE,
        Error::DbLocked { .. } => StatusCode::SERVICE_UNAVAILABLE,
        Error::Storage(_) => StatusCode::INTERNAL_SERVER_ERROR,
        Error::Embedder(_) | Error::EmbeddingMismatch { .. } => StatusCode::BAD_GATEWAY,
        Error::Llm(_) => StatusCode::BAD_GATEWAY,
        Error::BudgetExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
    }
}

/// Render one [`Error`] as `(status, {error, code})`.
pub fn render_error(e: Error) -> Response {
    let status = status_for(&e);
    (status, Json(api_error(&e))).into_response()
}

/// Map a `Result<T: Serialize, Error>` into a JSON response.
pub fn render_result<T: serde::Serialize>(r: crate::error::Result<T>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => render_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_mirrors_cli_taxonomy() {
        let cases = [
            (Error::Config("x".into()), "CONFIG", StatusCode::BAD_REQUEST),
            (
                Error::InvalidInput("x".into()),
                "INVALID_INPUT",
                StatusCode::BAD_REQUEST,
            ),
            (
                Error::SchemaTooNew {
                    db: 9,
                    supported: 8,
                },
                "SCHEMA_TOO_NEW",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                Error::DbLocked {
                    path: "a".into(),
                    pid: 1,
                },
                "DB_LOCKED",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                Error::Storage("x".into()),
                "STORAGE",
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                Error::Embedder("x".into()),
                "EMBEDDER",
                StatusCode::BAD_GATEWAY,
            ),
            (
                Error::EmbeddingMismatch {
                    model: "a".into(),
                    dim: 1,
                    found: "b".into(),
                    found_dim: 2,
                },
                "EMBEDDING_MISMATCH",
                StatusCode::BAD_GATEWAY,
            ),
            (Error::Llm("x".into()), "LLM", StatusCode::BAD_GATEWAY),
            (
                Error::BudgetExceeded {
                    used: 3,
                    ceiling: 2,
                },
                "BUDGET_EXCEEDED",
                StatusCode::TOO_MANY_REQUESTS,
            ),
        ];
        for (e, code, status) in cases {
            assert_eq!(error_code(&e), code);
            assert_eq!(status_for(&e), status);
            assert!(!e.to_string().is_empty());
        }
    }
}
