//! `serve` — run the MCP server over stdio or streamable HTTP (issue 0008).
//!
//! One process, one database: the server opens the `StoreHandle` once and
//! holds the single-writer flock for its lifetime, so a concurrent CLI run
//! fails with `DbLocked` (exit 4). Shutdown is graceful — the HTTP transport
//! stops accepting, in-flight requests drain, the lock releases on drop.
//!
//! Fail-closed: the bind/token pair (config + CLI overrides) is validated
//! *before* anything is opened; a tokenless non-loopback bind never starts.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::{Config, EmbedProvider};
use crate::defaults;
use crate::error::{Error, Result};
use crate::mcp::AgosServer;
use crate::storage::StoreHandle;

/// `serve` — run until stdin closes (`--stdio`) or the process is signalled.
pub async fn run_serve(
    cfg: &Config,
    stdio: bool,
    bind: Option<String>,
    token: Option<String>,
) -> Result<()> {
    // CLI flags override config; re-validate the *effective* pair so a
    // tokenless `--bind 0.0.0.0` is refused even when the config file would
    // have allowed the default loopback bind.
    let mut effective = cfg.clone();
    if let Some(b) = bind {
        effective.server.bind = b;
    }
    if let Some(t) = token {
        effective.server.token = t;
    }
    effective.validate()?;
    crate::server::validate_server_token(&effective.server.bind, &effective.server.token)
        .map_err(Error::Config)?;

    // The server is the database's single process: open once, hold the flock.
    let store = StoreHandle::open(&effective, defaults::READ_POOL_SIZE).await?;
    let dim = store.embed_dim().await?;
    let embedder: Arc<dyn crate::embed::Embedder> = match effective.embed.provider {
        // Degraded: keyword-only recall (D4); `NoEmbedder` has dim 0, which
        // must not fail the stored-dim validation the other providers need.
        EmbedProvider::None => Arc::new(crate::embed::NoEmbedder),
        _ => {
            let embedder = crate::embed::embedder_from_config(&effective.embed, dim);
            store.validate_embed_dim(&*embedder).await?;
            Arc::from(embedder)
        }
    };
    let chat = crate::llm::chat_from_config(&effective.llm, None);
    let server = AgosServer::new(store, effective.clone(), embedder, chat);

    tracing::info!(
        "agos-memory serve starting (transport: {}, bind: {}, agent: {})",
        if stdio { "stdio" } else { "http" },
        if stdio {
            "—"
        } else {
            effective.server.bind.as_str()
        },
        effective.agent_id
    );

    if stdio {
        return crate::server::stdio::run_stdio(server)
            .await
            .map_err(|e| Error::Storage(e.to_string()));
    }

    let http = crate::server::http::HttpServer {
        bind: effective.server.bind.clone(),
        token: effective.server.token.clone(),
        handler: server,
    };
    let shutdown = shutdown_token();
    http.serve(shutdown)
        .await
        .map_err(|e| Error::Storage(e.to_string()))
}

/// A cancellation token cancelled on SIGINT/SIGTERM, so the HTTP transport can
/// drain gracefully instead of dying mid-request.
fn shutdown_token() -> CancellationToken {
    let token = CancellationToken::new();
    let handle = token.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(term) => term,
                Err(e) => {
                    tracing::warn!("cannot install SIGTERM handler: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::info!("shutdown signal received");
        handle.cancel();
    });
    token
}
