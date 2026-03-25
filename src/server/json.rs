//! JSON HTTP routes for the memory API (issue 0002).
//!
//! The six routes mirror the MCP tools one-to-one — same `*Input` argument
//! structs, same outcome DTOs — delegating to one call site each
//! ([`crate::api::MemoryApi]). Failed calls render the CLI error taxonomy as
//! `{error, code}` via [`crate::server::api`], so operators see the same
//! vocabulary over the wire as on stderr.
//!
//! State: the `MemoryApi` is attached to the router with
//! `axum::extract::Extension` by the caller ([`crate::server::http::router`]),
//! keeping this module free of transport/auth concerns (auth wraps the whole
//! protected surface; `/healthz` stays public).

use axum::Router;
use axum::extract::{Extension, Json, Path};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::api::{
    ExplainInput, ForgetInput, MemoryApi, RecallInput, RememberInput, SummarizeInput,
};
use crate::server::api::{ApiError, render_error, render_result};

/// `forget` body carries the action fields; `id` arrives from the path
/// (`/api/v1/forget/{id}`), keeping the memory address in the URL and the
/// verb in the body. This matches the OpenAPI spec (issue 0008): the path
/// parameter is the source of truth, not an ignored body field.
#[derive(Debug, serde::Deserialize)]
struct ForgetBody {
    /// Action: soft | restore | hard | rollback (default soft).
    pub action: Option<String>,
    /// Rollback target version (`action = "rollback"` only).
    pub to_version: Option<i64>,
    /// Reason recorded in the forget audit ledger.
    pub reason: Option<String>,
}

/// Build the six memory routes (without state or auth — those are layered by
/// [`crate::server::http::router`]).
pub fn routes() -> Router {
    Router::new()
        .route("/remember", post(remember))
        .route("/recall", post(recall))
        .route("/summarize", post(summarize))
        .route("/forget/{id}", post(forget))
        .route("/explain/{id}", get(explain))
        .route("/status", get(status))
}

async fn remember(
    Extension(api): Extension<MemoryApi>,
    Json(args): Json<RememberInput>,
) -> Response {
    render_result(api.remember(&args).await)
}

async fn recall(Extension(api): Extension<MemoryApi>, Json(args): Json<RecallInput>) -> Response {
    render_result(api.recall(&args).await)
}

async fn summarize(
    Extension(api): Extension<MemoryApi>,
    Json(args): Json<SummarizeInput>,
) -> Response {
    render_result(api.summarize(&args).await)
}

async fn forget(
    Extension(api): Extension<MemoryApi>,
    Path(id): Path<String>,
    Json(body): Json<ForgetBody>,
) -> Response {
    let args = ForgetInput {
        id,
        action: body.action,
        to_version: body.to_version,
        reason: body.reason,
    };
    render_result(api.forget(&args).await)
}

async fn explain(Extension(api): Extension<MemoryApi>, Path(id): Path<String>) -> Response {
    match api.explain(&ExplainInput { id }).await {
        Ok(Some(report)) => Json(report).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: "memory not found".to_string(),
                code: "NOT_FOUND",
            }),
        )
            .into_response(),
        Err(e) => render_error(e),
    }
}

async fn status(Extension(api): Extension<MemoryApi>) -> Response {
    render_result(api.status().await)
}
