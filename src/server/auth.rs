//! Bearer-token auth layer for the streamable HTTP transport.
//!
//! Fail-closed policy, decided by [`authorize`] (pure, unit-tested) and
//! applied by the axum middleware:
//!
//! - token configured → **every** request must present
//!   `Authorization: Bearer <token>` (compared constant-time);
//! - no token configured → only a **loopback bind** may serve requests
//!   (local development / CI). The bind is the authority — never a
//!   client-controlled `Host`/`Origin` header, which is trivially spoofed.
//!   `Config::validate()` refuses a tokenless non-loopback bind at startup; this
//!   layer refuses it again at request time (defence in depth).

use std::sync::Arc;

use axum::{
    body::Body,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Outcome of the auth decision: pass, or the 401 body to send back.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AuthDecision {
    Allow,
    Reject(&'static str),
}

/// Decide whether one request may pass (pure; see module docs).
///
/// `authorization` is the raw `Authorization` header value, if any.
pub(crate) fn authorize(
    expected_token: &str,
    bind: &str,
    authorization: Option<&str>,
) -> AuthDecision {
    // A configured token gates every request, regardless of origin — this is
    // the primary control; the loopback check below only covers tokenless
    // binds.
    if !expected_token.is_empty() {
        return match authorization.and_then(bearer_credential) {
            Some(credential) if ct_eq(credential, expected_token) => AuthDecision::Allow,
            _ => AuthDecision::Reject("invalid token"),
        };
    }

    // No token configured: only loopback binds may serve. The *bind* is
    // server-owned state; client headers are never consulted, so a spoofed
    // `Host: 127.0.0.1` cannot widen a tokenless non-loopback bind.
    if crate::server::is_loopback(bind) {
        AuthDecision::Allow
    } else {
        AuthDecision::Reject("bearer token required for non-loopback bind")
    }
}

/// Extract the credential from an `Authorization: Bearer <credential>` header.
///
/// The scheme comparison is case-insensitive (RFC 9110 §11); anything else
/// (missing, wrong scheme, empty credential) yields `None`.
fn bearer_credential(value: &str) -> Option<&str> {
    let (scheme, credential) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer") || credential.trim().is_empty() {
        return None;
    }
    Some(credential.trim())
}

/// Constant-time equality: both sides are hashed to a fixed length and the
/// digests are XOR-accumulated, so neither length nor early bytes leak through
/// timing (plan 0008: reuse `sha2` via [`crate::util::sha256_hex`], no new dep).
fn ct_eq(a: &str, b: &str) -> bool {
    let (da, db) = (crate::util::sha256_hex(a), crate::util::sha256_hex(b));
    da.bytes()
        .zip(db.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Apply the auth middleware to the given router.
pub fn auth_middleware(app: axum::Router, expected_token: String, bind: String) -> axum::Router {
    let token = Arc::new(expected_token);
    let bind = Arc::new(bind);
    app.layer(axum::middleware::from_fn(
        move |request: Request<Body>, next: Next| {
            let token = token.clone();
            let bind = bind.clone();
            async move { auth_check(&token, &bind, request, next).await }
        },
    ))
}

async fn auth_check(token: &str, bind: &str, request: Request<Body>, next: Next) -> Response {
    let authorization = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    match authorize(token, bind, authorization) {
        AuthDecision::Allow => next.run(request).await,
        AuthDecision::Reject(reason) => (StatusCode::UNAUTHORIZED, reason).into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn authorize_token_configured_gates_every_request() {
        use crate::server::auth::{AuthDecision, authorize};
        let token = "test-secret-token-1234";
        // A configured token gates every request on every bind.
        assert_eq!(
            authorize(token, "0.0.0.0:8710", Some(&format!("Bearer {token}"))),
            AuthDecision::Allow
        );
        assert_eq!(
            authorize(token, "127.0.0.1:8710", Some(&format!("Bearer {token}"))),
            AuthDecision::Allow
        );
        // Missing / wrong scheme / wrong credential → 401, even on loopback.
        assert_eq!(
            authorize(token, "127.0.0.1:8710", None),
            AuthDecision::Reject("invalid token")
        );
        assert_eq!(
            authorize(token, "127.0.0.1:8710", Some("Basic dXNlcjpwYXNz")),
            AuthDecision::Reject("invalid token")
        );
        assert_eq!(
            authorize(token, "127.0.0.1:8710", Some("Bearer wrong-token-value")),
            AuthDecision::Reject("invalid token")
        );
        // Scheme is case-insensitive; the credential is not.
        assert_eq!(
            authorize(token, "127.0.0.1:8710", Some(&format!("bEaReR {token}"))),
            AuthDecision::Allow
        );
    }

    #[test]
    fn authorize_tokenless_only_serves_loopback_binds() {
        use crate::server::auth::{AuthDecision, authorize};
        // The *bind* decides — a spoofed `Host: 127.0.0.1` header must not
        // widen a tokenless non-loopback bind (headers are never consulted).
        assert_eq!(authorize("", "127.0.0.1:8710", None), AuthDecision::Allow);
        assert_eq!(authorize("", "localhost:8710", None), AuthDecision::Allow);
        assert_eq!(authorize("", "[::1]:8710", None), AuthDecision::Allow);
        assert_eq!(
            authorize("", "0.0.0.0:8710", None),
            AuthDecision::Reject("bearer token required for non-loopback bind")
        );
        assert_eq!(
            authorize("", "0.0.0.0:8710", Some("Bearer whatever")),
            AuthDecision::Reject("bearer token required for non-loopback bind")
        );
    }

    #[test]
    fn ct_eq_is_exact_and_length_agnostic() {
        use crate::server::auth::ct_eq;
        assert!(ct_eq("same", "same"));
        assert!(ct_eq("", ""));
        assert!(!ct_eq("same", "sam "));
        assert!(!ct_eq("short", "a-much-longer-value"));
        assert!(!ct_eq("", "x"));
    }

    #[test]
    fn bearer_credential_parses_strictly() {
        use crate::server::auth::bearer_credential;
        assert_eq!(bearer_credential("Bearer tok"), Some("tok"));
        assert_eq!(bearer_credential("bearer tok"), Some("tok"));
        assert_eq!(bearer_credential("BEARER  tok  "), Some("tok"));
        assert_eq!(bearer_credential("Bearer"), None);
        assert_eq!(bearer_credential("Bearer "), None);
        assert_eq!(bearer_credential("Basic tok"), None);
        assert_eq!(bearer_credential(""), None);
    }

    #[test]
    fn loopback_accepts_empty_token() {
        assert!(crate::server::validate_server_token("127.0.0.1:8080", "").is_ok());
        assert!(crate::server::validate_server_token("localhost:8080", "").is_ok());
        assert!(crate::server::validate_server_token("[::1]:8080", "").is_ok());
    }

    #[test]
    fn non_loopback_requires_token() {
        assert!(crate::server::validate_server_token("0.0.0.0:8080", "").is_err());
        assert!(crate::server::validate_server_token("example.com:8080", "").is_err());
        assert!(crate::server::validate_server_token("192.168.1.1:8080", "").is_err());
        assert!(crate::server::validate_server_token("0.0.0.0:8080", "short").is_err());
        assert!(
            crate::server::validate_server_token("0.0.0.0:8080", "this-is-long-enough").is_ok()
        );
    }

    #[test]
    fn is_loopback_classifies_correctly() {
        let loopback = [
            "127.0.0.1",
            "127.9.9.9:8710",
            "localhost",
            "LOCALHOST:8080",
            "::1",
            "[::1]",
            "[::1]:8710",
        ];
        let not_loopback = [
            "",
            "0.0.0.0",
            "0.0.0.0:8080",
            "::",
            "[::]",
            // Ambiguous (an unbracketed `addr:port` reads as the IPv6 literal
            // `::1:8080`); fail-closed is correct, the port form is `[::1]:8080`.
            "::1:8080",
            "example.com",
            "example.com:8080",
            "192.168.1.1",
            "10.0.0.2:8710",
        ];
        for bind in loopback {
            assert!(crate::server::is_loopback(bind), "{bind} must be loopback");
        }
        for bind in not_loopback {
            assert!(
                !crate::server::is_loopback(bind),
                "{bind} must not be loopback"
            );
        }
    }

    #[test]
    fn validate_server_token_tracks_config_validate() {
        // The server-side rule must agree with `Config::validate()` for every
        // bind the config layer accepts or refuses (single source of truth:
        // both call `is_loopback`).
        for bind in ["127.0.0.1:8710", "localhost:8710", "[::1]:8710"] {
            assert!(crate::server::validate_server_token(bind, "").is_ok());
            assert!(crate::server::validate_server_token(bind, "short").is_ok());
        }
        for bind in ["0.0.0.0:8080", "example.com:8080", "192.168.1.1:8710"] {
            assert!(crate::server::validate_server_token(bind, "").is_err());
            assert!(crate::server::validate_server_token(bind, "short").is_err());
            assert!(crate::server::validate_server_token(bind, "this-is-long-enough").is_ok());
        }
    }
}
