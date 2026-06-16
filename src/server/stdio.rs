//! Stdio MCP transport: speaks JSON-RPC over stdin/stdout to an MCP client.
//!
//! The rmcp `transport-io` feature provides `stdio()` which returns a
//! `(tokio::io::Stdin, tokio::io::Stdout)` pair wired into the JSON-RPC
//! transport. The server runs until the client disconnects (stdin closes).

use std::sync::Arc;

use crate::mcp::AgosServer;
use crate::server::ServerError;

/// Run the MCP server over the stdio transport.
///
/// The server listens for JSON-RPC requests on stdin and writes responses
/// to stdout. It runs until the client disconnects (stdin closes) or the
/// process receives a cancellation signal.
pub async fn run_stdio(handler: AgosServer) -> Result<(), ServerError> {
    let transport = rmcp::transport::io::stdio();
    let service = Arc::new(handler);

    match rmcp::serve_server(service, transport).await {
        Ok(running) => match running.waiting().await {
            Ok(_quit_reason) => Ok(()),
            Err(e) if e.is_panic() => Err(ServerError::Panic),
            Err(_) => Err(ServerError::JoinError),
        },
        Err(_) => Err(ServerError::JoinError),
    }
}
