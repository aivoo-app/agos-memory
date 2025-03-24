//! Layered configuration (issue 0002).
//!
//! Precedence (lowest wins first):
//! 1. built-in defaults
//! 2. TOML file (`--config`, default `./agos-memory.toml` if present)
//! 3. environment (`AGOS_MEMORY_*`, e.g. `AGOS_MEMORY_DB_PATH`)
//! 4. explicit CLI flags (applied by the caller on the returned struct)
//!
//! Secrets (API keys, bearer tokens) are held as plain values but must never
//! be logged; `Debug` for [`Config`] redacts them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Embedding provider selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbedProvider {
    /// OpenAI-compatible `/v1/embeddings` endpoint (default; point at agos-proxy).
    #[serde(rename = "openai_compat")]
    OpenAiCompat,
    /// No embeddings: degraded keyword-only mode (FTS5).
    None,
}

/// Embedding configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedConfig {
    /// Which provider backs embeddings.
    pub provider: EmbedProvider,
    /// Base URL for `openai_compat` (agos-proxy by default).
    pub base_url: String,
    /// Model name as the provider knows it.
    pub model: String,
    /// Bearer token; redacted in logs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
}

impl Default for EmbedConfig {
    fn default() -> Self {
        Self {
            provider: EmbedProvider::OpenAiCompat,
            base_url: "http://127.0.0.1:8080".into(),
            model: "text-embedding-3-small".into(),
            api_key: String::new(),
            timeout_secs: 30,
        }
    }
}

/// LLM (chat) configuration for extraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Base URL for an OpenAI-compatible chat endpoint (agos-proxy).
    pub base_url: String,
    /// Model used for extraction.
    pub model: String,
    /// Bearer token; redacted in logs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".into(),
            model: "gpt-4o-mini".into(),
            api_key: String::new(),
            timeout_secs: 60,
        }
    }
}

/// Server configuration (`serve` command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Bind address. 127.0.0.1 by default; non-loopback requires a token (fail closed).
    pub bind: String,
    /// Bearer token required for non-loopback binds; redacted in logs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8710".into(),
            token: String::new(),
        }
    }
}

/// Top-level configuration.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Path to the SQLite database file.
    pub db_path: PathBuf,
    /// Agent identity owning this database (v0.1.0: informational, single-agent).
    pub agent_id: String,
    /// Embedding settings.
    pub embed: EmbedConfig,
    /// Extraction LLM settings.
    pub llm: LlmConfig,
    /// HTTP server settings.
    pub server: ServerConfig,
    /// RUST_LOG-style filter for tracing.
    pub log_filter: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("memory.db"),
            agent_id: "default".into(),
            embed: EmbedConfig::default(),
            llm: LlmConfig::default(),
            server: ServerConfig::default(),
            log_filter: "info".into(),
        }
    }
}

impl Config {
    /// Load with the documented precedence: defaults <- file <- env.
    ///
    /// A missing config file is fine — defaults apply.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut cfg = Config::default();

        let file = path
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("agos-memory.toml"));
        if file.is_file() {
            let raw = std::fs::read_to_string(&file)
                .map_err(|e| Error::Config(format!("cannot read {}: {e}", file.display())))?;
            let parsed: Config = toml::from_str(&raw)
                .map_err(|e| Error::Config(format!("invalid TOML in {}: {e}", file.display())))?;
            cfg = parsed;
        }

        cfg.apply_env();
        cfg.validate()?;
        Ok(cfg)
    }

    /// Apply `AGOS_MEMORY_*` environment overrides.
    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("AGOS_MEMORY_DB_PATH") {
            self.db_path = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_AGENT_ID") {
            self.agent_id = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_LOG") {
            self.log_filter = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_EMBED_BASE_URL") {
            self.embed.base_url = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_EMBED_MODEL") {
            self.embed.model = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_EMBED_API_KEY") {
            self.embed.api_key = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_LLM_BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_LLM_MODEL") {
            self.llm.model = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_LLM_API_KEY") {
            self.llm.api_key = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_BIND") {
            self.server.bind = v;
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_TOKEN") {
            self.server.token = v;
        }
    }

    /// Validate invariants. Fail closed on unsafe server exposure.
    pub fn validate(&self) -> Result<()> {
        if self.db_path.as_os_str().is_empty() {
            return Err(Error::Config("db_path must not be empty".into()));
        }
        if self.agent_id.trim().is_empty() {
            return Err(Error::Config("agent_id must not be empty".into()));
        }
        if !self.server.bind.starts_with("127.0.0.1") && !self.server.bind.starts_with("[::1]") {
            if self.server.token.is_empty() {
                return Err(Error::Config(format!(
                    "binding to a non-loopback address ({}) requires a token \
                     (set AGOS_MEMORY_TOKEN or server.token); refusing to start",
                    self.server.bind
                )));
            }
            if self.server.token.len() < 16 {
                return Err(Error::Config(
                    "server.token must be at least 16 characters".into(),
                ));
            }
        }
        Ok(())
    }
}

/// Redacted `Debug` so secrets never reach logs.
impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("db_path", &self.db_path)
            .field("agent_id", &self.agent_id)
            .field("embed.provider", &self.embed.provider)
            .field("embed.base_url", &self.embed.base_url)
            .field("embed.model", &self.embed.model)
            .field("embed.api_key", &redact(&self.embed.api_key))
            .field("llm.base_url", &self.llm.base_url)
            .field("llm.model", &self.llm.model)
            .field("llm.api_key", &redact(&self.llm.api_key))
            .field("server.bind", &self.server.bind)
            .field("server.token", &redact(&self.server.token))
            .field("log_filter", &self.log_filter)
            .finish()
    }
}

fn redact(v: &str) -> &str {
    if v.is_empty() { "" } else { "<redacted>" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn non_loopback_requires_token() {
        let mut cfg = Config::default();
        cfg.server.bind = "0.0.0.0:8710".into();
        assert!(cfg.validate().is_err());

        cfg.server.token = "short".into();
        assert!(cfg.validate().is_err());

        cfg.server.token = "a- reasonably long token value".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut cfg = Config::default();
        cfg.embed.api_key = "sk-super-secret".into();
        cfg.server.token = "bearer-super-secret".into();
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("sk-super-secret"),
            "leaked api key: {rendered}"
        );
        assert!(
            !rendered.contains("bearer-super-secret"),
            "leaked token: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn parse_toml_minimal() {
        let cfg: Config = toml::from_str("db_path = '/tmp/x.db'").unwrap();
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/x.db"));
        cfg.validate().unwrap();
    }

    #[test]
    fn load_missing_file_uses_defaults() {
        let cfg = Config::load(Some(Path::new("/nonexistent/agos-memory.toml"))).unwrap();
        assert_eq!(cfg.agent_id, "default");
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(
            &path,
            "db_path = '/tmp/from-file.db'\nagent_id = 'shahriar'\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.agent_id, "shahriar");
        assert_eq!(cfg.db_path, PathBuf::from("/tmp/from-file.db"));
    }

    #[test]
    fn test_none_provider_deserialization() {
        let cfg: Config = toml::from_str(
            r#"db_path = '/tmp/x.db'
agent_id = 'test-none'

[embed]
provider = 'none'

[llm]
base_url = 'http://localhost'
model = 'gpt-test'

[server]
bind = '127.0.0.1:9999'
"#,
        )
        .unwrap();
        assert_eq!(cfg.embed.provider, EmbedProvider::None);
        // base_url is missing from TOML, so serde uses EmbedConfig::default() which is "http://127.0.0.1:8080"
        assert_eq!(cfg.embed.base_url, "http://127.0.0.1:8080");
        assert_eq!(cfg.agent_id, "test-none");
        cfg.validate().unwrap();
    }
}
