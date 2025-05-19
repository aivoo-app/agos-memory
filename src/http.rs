//! Shared HTTP client for provider calls (issue 0023).
//!
//! Both `openai_compat` providers (embeddings + chat) talk to agos-proxy over
//! HTTP. This module owns the one configured client: rustls TLS, JSON bodies,
//! bearer auth, per-request timeouts. No retries here — the jobs layer (0026)
//! owns retry/backoff policy.

use std::time::Duration;

use serde::Serialize;

use crate::error::{Error, Result};

/// Connection settings for one provider endpoint.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// Base URL, e.g. `http://127.0.0.1:8080`.
    pub base_url: String,
    /// Bearer token; sent as `Authorization: Bearer …` when non-empty.
    pub api_key: String,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl HttpConfig {
    /// Build from parts; trims a trailing `/` off the base URL.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, timeout_secs: u64) -> Self {
        let mut base = base_url.into();
        while base.ends_with('/') {
            base.pop();
        }
        Self {
            base_url: base,
            api_key: api_key.into(),
            timeout: Duration::from_secs(timeout_secs.max(1)),
        }
    }
}

/// POST `url` with a JSON body; deserialize the JSON response.
///
/// `kind` names the caller for error messages ("embeddings" / "chat").
pub async fn post_json<Req: Serialize, Resp: serde::de::DeserializeOwned>(
    url: &str,
    body: &Req,
    cfg: &HttpConfig,
    kind: &'static str,
    map_err: fn(String) -> Error,
) -> Result<Resp> {
    let client = reqwest::Client::builder()
        .timeout(cfg.timeout)
        .build()
        .map_err(|e| map_err(format!("cannot build HTTP client: {e}")))?;
    let mut req = client.post(url).json(body);
    if !cfg.api_key.is_empty() {
        req = req.bearer_auth(&cfg.api_key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| map_err(format!("{kind} request to {url} failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let clipped: String = body.chars().take(300).collect();
        return Err(map_err(format!(
            "{kind} request to {url} returned HTTP {status}: {clipped}"
        )));
    }
    resp.json::<Resp>()
        .await
        .map_err(|e| map_err(format!("{kind} response from {url} is not valid JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_error_is_typed() {
        let cfg = HttpConfig::new("http://127.0.0.1:1", "", 2);
        let err = post_json::<_, serde_json::Value>(
            "http://127.0.0.1:1/v1/embeddings",
            &serde_json::json!({}),
            &cfg,
            "embeddings",
            Error::Embedder,
        )
        .await
        .expect_err("unroutable host must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("127.0.0.1:1"),
            "error must name the URL, got: {msg}"
        );
    }

    #[test]
    fn trims_trailing_slash() {
        let cfg = HttpConfig::new("http://x:8080//", "", 5);
        assert_eq!(cfg.base_url, "http://x:8080");
    }
}
