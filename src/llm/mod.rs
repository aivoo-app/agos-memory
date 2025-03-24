//! LLM chat clients (issue 0013).
//!
//! Extraction (v0.2.0) needs an LLM. The [`ChatClient`] trait abstracts the
//! provider; [`MockChat`] gives deterministic responses for tests and the
//! offline eval harness. The real `openai_compat` client (against
//! agos-proxy) lands with the extractor in v0.2.0.

use async_trait::async_trait;

use crate::error::{Error, Result};

/// A chat-completion-style client for extraction prompts.
#[async_trait]
pub trait ChatClient: Send + Sync {
    /// Send a single-turn prompt and return the assistant text.
    async fn complete(&self, prompt: &str) -> Result<String>;

    /// Model identifier, recorded with every ledger entry.
    fn model(&self) -> &str;
}

/// Deterministic mock: echoes a canned JSON response. Versioned by tag so
/// replay audits can distinguish mock runs from real extractions.
pub struct MockChat {
    response: String,
    model: String,
}

impl MockChat {
    /// Mock that always returns `response`.
    pub fn fixed(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            model: "mock-chat-v1".into(),
        }
    }
}

impl Default for MockChat {
    fn default() -> Self {
        Self::fixed(r#"{"memories":[]}"#)
    }
}

#[async_trait]
impl ChatClient for MockChat {
    async fn complete(&self, _prompt: &str) -> Result<String> {
        Ok(self.response.clone())
    }

    fn model(&self) -> &str {
        &self.model
    }
}

/// Validate that a response looks like JSON (used before parsing extraction output).
pub fn ensure_json(response: &str) -> Result<&str> {
    let trimmed = response.trim();
    serde_json::from_str::<serde_json::Value>(trimmed)
        .map_err(|e| Error::Llm(format!("response is not valid JSON: {e}")))?;
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_returns_fixed_response() {
        let c = MockChat::fixed("{\"ok\":true}");
        assert_eq!(c.complete("anything").await.unwrap(), "{\"ok\":true}");
        assert_eq!(c.model(), "mock-chat-v1");
    }

    #[tokio::test]
    async fn mock_default_is_empty_extraction() {
        let c = MockChat::default();
        let out = c.complete("extract facts").await.unwrap();
        assert!(ensure_json(&out).is_ok());
    }

    #[test]
    fn ensure_json_rejects_garbage() {
        assert!(ensure_json("this is not json").is_err());
        assert!(ensure_json(" {\"a\": 1} ").is_ok());
    }
}
