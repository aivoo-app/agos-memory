//! Streamable HTTP MCP transport with bearer-token auth.
//!
//! Listens on a TCP socket and speaks the MCP streamable-HTTP protocol (SEP-2567)
//! via an axum router. Every request passes through the auth middleware, which
//! rejects non-loopback origins without a token (401).

use std::sync::Arc;

use axum::Router;
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
        tracing::info!("MCP HTTP server listening on {addr}");

        // Build the session manager (in-memory, no external store).
        let session_manager = Arc::new(LocalSessionManager::default());

        // Configure the streamable HTTP server.
        let config = StreamableHttpServerConfig::default();

        // Build the service: a factory that clones the handler per request.
        let handler = self.handler;
        let service_factory = {
            let handler = handler.clone();
            move || -> Result<AgosServer, std::io::Error> { Ok(handler.clone()) }
        };

        let service: StreamableHttpService<AgosServer, LocalSessionManager> =
            StreamableHttpService::new(service_factory, session_manager, config);

        // Wrap in an axum router at /mcp with the auth middleware.
        let token = self.token.clone();
        let bind = self.bind.clone();
        let app = crate::server::auth::auth_middleware(
            Router::new().nest_service("/mcp", service),
            token,
            bind,
        );

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
}
