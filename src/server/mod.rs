//! MCP server transports: stdio and streamable HTTP, plus auth helpers.
//!
//! Both transports wrap the same `AgosServer` tool handler from [`crate::mcp`].
//! Stdio speaks newline-delimited JSON-RPC to an MCP client on stdin/stdout;
//! HTTP speaks the streamable-HTTP MCP protocol over an axum router with
//! bearer-token auth (fail-closed: unknown origin or missing token → 401).

pub mod auth;
pub mod http;
pub mod stdio;

/// Errors that can occur while running an MCP server (either transport).
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// The server task panicked.
    #[error("server panicked")]
    Panic,

    /// Joining the server task failed.
    #[error("server join error")]
    JoinError,

    /// Auth validation failed (e.g. non-loopback bind with short token).
    #[error("auth validation: {0}")]
    Auth(String),

    /// Could not bind to the specified address.
    #[error("cannot bind to {addr}: {source}")]
    Bind {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    /// Could not get the local address after binding.
    #[error("cannot determine local address: {0}")]
    Listen(#[from] std::io::Error),
}

/// Extract the host part of a bind string: `host`, `host:port`, or `[v6]:port`.
///
/// IPv6 literals are bracket-stripped; a bare IPv6 address (`::1`) passes
/// through whole. The port is only stripped when the suffix after the final
/// `:` is all digits (a port), so `::1` is not mistaken for host `::` plus
/// port `1`.
fn host_of(bind: &str) -> &str {
    if let Some(rest) = bind.strip_prefix('[') {
        // `[::1]:8710` → `::1`; unterminated bracket → take what is there.
        return match rest.split_once(']') {
            Some((host, _)) => host,
            None => rest,
        };
    }
    // A bare IPv6 address contains several `:` and has no port — leave it
    // whole. Only a single `:` can separate host from port.
    if let Some((host, _)) = bind.split_once(':').filter(|(host, port)| {
        !host.contains(':') && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
    }) {
        return host;
    }
    bind
}

/// Check whether `bind` (host, `host:port`, or `[v6]:port`) is loopback-only.
///
/// Classification is by parsed IP address (`IpAddr::is_loopback`) with a
/// `localhost` hostname fallback:
///
/// - loopback: `127.0.0.1`, `127.9.9.9`, `::1`, `[::1]`, `[::1]:8710`,
///   `localhost`, `localhost:8710`
/// - **not** loopback: `""`, `0.0.0.0` (wildcard — reachable from any
///   interface, so it must require a token), `::`, `example.com`,
///   `192.168.1.1`
///
/// This is the single source of truth for the loopback rule: `Config::validate`
/// (fail-closed on tokenless non-loopback binds) and the HTTP auth layer both
/// call it, so the two layers cannot drift.
pub fn is_loopback(bind: &str) -> bool {
    let host = host_of(bind);
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => host.eq_ignore_ascii_case("localhost"),
    }
}

/// Validate the server token for a given bind address.
///
/// Non-loopback binds require a token of at least 16 characters;
/// loopback binds accept an empty token (local-only). This mirrors the rule
/// enforced by `Config::validate()` so the two layers stay in lockstep.
pub fn validate_server_token(bind: &str, token: &str) -> Result<(), String> {
    if is_loopback(bind) {
        return Ok(());
    }
    if token.len() < 16 {
        return Err(format!(
            "binding to non-loopback address {bind} requires a token of at least 16 characters"
        ));
    }
    Ok(())
}
