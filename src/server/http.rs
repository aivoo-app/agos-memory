//! Streamable HTTP MCP transport with bearer-token auth.
//!
//! Listens on a TCP socket and speaks the MCP streamable-HTTP protocol (SEP-2567)
//! via an axum router. Every request passes through the auth middleware, which
//! rejects non-loopback origins without a token (401).

use std::sync::Arc;

use axum::Router;
use axum::{Json, http::StatusCode, routing::get};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::mcp::AgosServer;
use crate::server::{ServerError, validate_server_token};

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

/// Builder that holds everything needed to start the HTTP server.
pub struct HttpServer {
    /// TCP address to bind to.
    pub bind: String,
    /// Bearer token (empty = loopback-only, no auth).
    pub token: String,
    /// MCP tool handler.
    pub handler: AgosServer,
}

impl HttpServer {
    /// Validate the bind address and token before starting.
    pub fn validate(&self) -> Result<(), String> {
        validate_server_token(&self.bind, &self.token)
    }

    /// Start the HTTP server and drive it until `shutdown` is cancelled.
    ///
    /// The caller is responsible for cancelling `shutdown` (typically via a
    /// SIGINT/SIGTERM handler) to gracefully stop the server.
    pub async fn serve(self, shutdown: CancellationToken) -> Result<(), ServerError> {
        // Validate before binding.
        self.validate().map_err(ServerError::Auth)?;

        // Bind the TCP listener.
        let listener = TcpListener::bind(&self.bind)
            .await
            .map_err(|e| ServerError::Bind {
                addr: self.bind.clone(),
                source: e,
            })?;

        let addr = listener.local_addr().map_err(ServerError::Listen)?;
        tracing::info!("HTTP server listening on {addr} (MCP + JSON API)");

        // Build the full router: public `/healthz` plus bearer-protected
        // `/mcp` and `/api/v1` on one app (single bind, single auth layer, single
        // shutdown path — D38).
        let api = self.handler.memory_api();
        let app = Self::router(self.handler.clone(), api, &self.token, &self.bind);

        // Spawn the axum serve loop; it exits when `shutdown` is cancelled.
        let shutdown_clone = shutdown.clone();
        let serve_handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_clone.cancelled_owned())
                .await
        });

        serve_handle.await.map_err(|e| {
            if e.is_panic() {
                ServerError::Panic
            } else {
                ServerError::JoinError
            }
        })??;
        Ok(())
    }

    /// Build the complete axum router for one `HttpServer` bind.
    ///
    /// `/healthz` is mounted *outside* the auth middleware (liveness must work
    /// without a token, even on non-loopback binds). `/mcp` (the streamable
    /// HTTP MCP service) and `/api/v1` (the JSON memory API) are nested behind
    /// the bearer middleware and share the [`MemoryApi`] via `Extension`.
    pub fn router(
        handler: crate::mcp::AgosServer,
        api: crate::api::MemoryApi,
        token: &str,
        bind: &str,
    ) -> Router {
        // MCP: session manager (in-memory) + streamable-HTTP service.
        let session_manager = Arc::new(LocalSessionManager::default());
        let config = StreamableHttpServerConfig::default();
        let mcp: StreamableHttpService<AgosServer, LocalSessionManager> =
            StreamableHttpService::new(
                move || -> Result<AgosServer, std::io::Error> { Ok(handler.clone()) },
                session_manager,
                config,
            );

        // Protected surface: MCP tunnel + JSON routes + shared API state.
        let protected = Router::new()
            .nest_service("/mcp", mcp)
            .nest("/api/v1", crate::server::json::routes())
            .layer(axum::extract::Extension(api));

        let protected =
            crate::server::auth::auth_middleware(protected, token.to_string(), bind.to_string());

        // `/healthz` is public; everything else is under `/`.
        Router::new()
            .route("/healthz", get(healthz))
            .nest("/", protected)
    }
}

/// Public liveness probe — no auth, no state, always 200.
async fn healthz() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}
