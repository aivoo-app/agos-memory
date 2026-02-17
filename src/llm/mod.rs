//! LLM chat clients (issue 0013).
//!
//! Extraction (v0.2.0) needs an LLM. The [`ChatClient`] trait abstracts the
//! provider; [`MockChat`] gives deterministic responses for tests and the
//! offline eval harness. The real `openai_compat` client (against
//! agos-proxy) lands with the extractor in v0.2.0.

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::http::{HttpConfig, post_json};

use std::time::Instant;

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

/// OpenAI-compatible chat provider (agos-proxy `/v1/chat/completions`).
pub struct OpenAiCompatChat {
    cfg: HttpConfig,
    model: String,
    ledger: Option<crate::embed::LedgerSink>,
}

impl OpenAiCompatChat {
    /// Build for `model`.
    pub fn new(cfg: HttpConfig, model: impl Into<String>) -> Self {
        Self {
            cfg,
            model: model.into(),
            ledger: None,
        }
    }

    /// Attach a ledger sink (one entry per `complete` call).
    pub fn with_ledger(mut self, sink: crate::embed::LedgerSink) -> Self {
        self.ledger = Some(sink);
        self
    }
}

#[derive(serde::Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(default)]
    temperature: f32,
}

#[derive(serde::Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(serde::Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
}

#[derive(serde::Deserialize)]
struct ChatChoice {
    #[serde(default)]
    message: ChatContent,
}

#[derive(serde::Deserialize, Default)]
struct ChatContent {
    #[serde(default)]
    content: String,
}

#[async_trait]
impl ChatClient for OpenAiCompatChat {
    async fn complete(&self, prompt: &str) -> Result<String> {
        let started = Instant::now();
        let url = format!("{}/v1/chat/completions", self.cfg.base_url);
        let body = ChatRequest {
            model: &self.model,
            messages: vec![ChatMessage {
                role: "user",
                content: prompt,
            }],
            temperature: 0.0,
        };
        let resp: ChatResponse = post_json(&url, &body, &self.cfg, "chat", Error::Llm).await?;
        let text = resp
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                Error::Llm(format!(
                    "chat response from {url} has no choices[0].message.content"
                ))
            })?;
        if let Some(sink) = &self.ledger {
            sink(crate::observe::ledger::estimated_entry(
                crate::observe::ledger::Purpose::Extract,
                &self.model,
                prompt,
                &text,
                started.elapsed().as_millis() as u64,
                true,
            ));
        }
        Ok(text)
    }

    fn model(&self) -> &str {
        &self.model
    }
}

/// Build a chat client from the configured provider.
///
/// `provider = "none"` (or an empty `base_url` when offline) yields
/// [`MockChat`], so the offline eval harness and CI never touch a network.
/// Otherwise an [`OpenAiCompatChat`] is built against the configured
/// base_url/model/api_key. The ledger sink is attached when one is given.
pub fn chat_from_config(
    cfg: &crate::config::LlmConfig,
    ledger: Option<crate::embed::LedgerSink>,
) -> std::sync::Arc<dyn ChatClient> {
    if cfg.base_url.trim().is_empty() {
        return std::sync::Arc::new(MockChat::default());
    }
    let http =
        crate::http::HttpConfig::new(cfg.base_url.clone(), cfg.api_key.clone(), cfg.timeout_secs);
    let model = if cfg.model.is_empty() {
        "gpt-4o-mini".to_string()
    } else {
        cfg.model.clone()
    };
    let mut chat = OpenAiCompatChat::new(http, model);
    if let Some(sink) = ledger {
        chat = chat.with_ledger(sink);
    }
    std::sync::Arc::new(chat)
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
