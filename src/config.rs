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

/// Session lifecycle configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Minutes of turn inactivity after which a session is closed as idle.
    pub idle_minutes: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self { idle_minutes: 30 }
    }
}

/// Per-session token budget configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Max LLM tokens chargeable to one session before recall degrades.
    pub max_tokens_per_session: u64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_tokens_per_session: 50_000,
        }
    }
}

/// Recall (read-path) tuning — v0.3.0.
///
/// Controls hybrid retrieval, rerank weights, per-tier half-life decay, token
/// packing, and the trust policy. See plan/DECISIONS.md (D24–D29) and
/// `docs/recall.md`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallConfig {
    /// Number of ranked items to return before packing (D28).
    pub top_k: usize,
    /// Soft token ceiling for one recall, enforced by whole-item packing (D25).
    pub budget_tokens: u64,
    /// Minimum rerank score for an item to be considered a hit (D26 no-hit).
    pub min_score: f32,
    /// Whether episodic-tier memories are eligible (D26).
    pub include_episodic: bool,
    /// Whether `pending` memories are eligible (D28).
    pub include_pending: bool,
    /// Trust policy: Strict excludes untrusted unless opted in (D29).
    pub trust_policy: TrustPolicy,
    /// Rerank weight splits (D23).
    pub weights: RecallWeights,
    /// Per-tier half-lives in hours; `0` = no decay (D24).
    pub half_life: RecallHalfLives,
    /// Fraction of `budget_tokens` reserved per tier (D25). Unused share rolls
    /// down to the next tier in declared order.
    pub budget_split: BudgetSplit,
}

/// Trust handling during recall (D29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TrustPolicy {
    /// `trusted` + `system` only; `--include-untrusted` opts into fenced hits.
    #[default]
    Strict,
    /// Untrusted memories are retrieved and rendered inside a fence as data.
    Fenced,
}

/// Rerank weight splits (D23). They need not sum to 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallWeights {
    pub sim: f32,
    pub importance: f32,
    pub decay: f32,
}

impl Default for RecallWeights {
    fn default() -> Self {
        Self {
            sim: 0.60,
            importance: 0.25,
            decay: 0.15,
        }
    }
}

/// Per-tier half-life in hours. `0` means no decay (D24).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecallHalfLives {
    pub working_hours: f64,
    pub episodic_hours: f64,
    pub semantic_hours: f64,
    pub procedural_hours: f64,
}

impl Default for RecallHalfLives {
    fn default() -> Self {
        Self {
            working_hours: 6.0,
            episodic_hours: 21.0 * 24.0,
            semantic_hours: 0.0,
            procedural_hours: 0.0,
        }
    }
}

/// Fraction of the token budget reserved per tier (must sum to ~1.0) (D25).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetSplit {
    pub working: f64,
    pub episodic: f64,
    pub semantic: f64,
    pub procedural: f64,
}

impl Default for BudgetSplit {
    fn default() -> Self {
        Self {
            working: 0.40,
            episodic: 0.30,
            semantic: 0.20,
            procedural: 0.10,
        }
    }
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            top_k: 8,
            budget_tokens: 1500,
            min_score: 0.35,
            include_episodic: false,
            include_pending: false,
            trust_policy: TrustPolicy::Strict,
            weights: RecallWeights::default(),
            half_life: RecallHalfLives::default(),
            budget_split: BudgetSplit::default(),
        }
    }
}

/// Memory write-path tuning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Extracted candidates below this confidence are stored as `pending`
    /// instead of `active` (default 0.4).
    pub pending_threshold: f64,
    /// Cosine similarity above which a candidate is treated as a duplicate of
    /// an existing memory and bumps its reference count (default 0.92).
    pub dedup_threshold: f64,
    /// Days after which a memory is eligible for automatic summarization.
    /// Set to 0 to disable for this tier (default: working=7, episodic=30,
    /// semantic=90, procedural=180).
    pub summarize_after_days_working: u32,
    pub summarize_after_days_episodic: u32,
    pub summarize_after_days_semantic: u32,
    pub summarize_after_days_procedural: u32,
    /// TTL in days per tier. 0 = never expire.
    /// Defaults: working=30, episodic=90, semantic=0, procedural=0.
    pub ttl_working_days: u32,
    pub ttl_episodic_days: u32,
    pub ttl_semantic_days: u32,
    pub ttl_procedural_days: u32,
    /// Grace period in days after soft deprecate before hard purge.
    /// Default: 30.
    pub ttl_grace_days: u32,
    /// Hour of day (UTC) to run the TTL reaper.
    /// Default: 3.
    pub reaper_hour: u32,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            pending_threshold: 0.4,
            dedup_threshold: 0.92,
            summarize_after_days_working: 7,
            summarize_after_days_episodic: 30,
            summarize_after_days_semantic: 90,
            summarize_after_days_procedural: 180,
            ttl_working_days: 30,
            ttl_episodic_days: 90,
            ttl_semantic_days: 0,
            ttl_procedural_days: 0,
            ttl_grace_days: 30,
            reaper_hour: 3,
        }
    }
}

impl MemoryConfig {
    /// Get the summarize_after_days threshold for a given tier.
    pub fn summarize_after_days_for_tier(&self, tier: &str) -> u32 {
        match tier {
            "working" => self.summarize_after_days_working,
            "episodic" => self.summarize_after_days_episodic,
            "semantic" => self.summarize_after_days_semantic,
            "procedural" => self.summarize_after_days_procedural,
            _ => 0,
        }
    }

    /// Get the TTL in days for a given tier. 0 = never expire.
    pub fn ttl_for_tier(&self, tier: &str) -> u32 {
        match tier {
            "working" => self.ttl_working_days,
            "episodic" => self.ttl_episodic_days,
            "semantic" => self.ttl_semantic_days,
            "procedural" => self.ttl_procedural_days,
            _ => 0,
        }
    }

    /// Grace period in days after soft deprecate before hard purge.
    pub fn grace_days(&self) -> u32 {
        self.ttl_grace_days
    }

    /// Hour of day (UTC) to run the TTL reaper.
    pub fn reaper_hour(&self) -> u32 {
        self.reaper_hour
    }
}

/// Consolidation job configuration (issue 0045).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsolidateConfig {
    /// Enable the periodic consolidation job.
    pub enabled: bool,
    /// Day of week (0=Sunday) to run the consolidation job.
    pub day: u32,
    /// Hour of day (UTC) to run the consolidation job.
    pub hour: u32,
}

impl Default for ConsolidateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            day: 0,  // Sunday
            hour: 4, // 4 AM UTC
        }
    }
}

/// Top-level configuration.
///
/// `PartialEq` only — `MemoryConfig` carries `f64` thresholds, which are not
/// `Eq`. Nothing keys a map on a `Config`, so this is not a loss.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
    /// Session lifecycle settings.
    pub session: SessionConfig,
    /// Per-session token budget settings.
    pub budget: BudgetConfig,
    /// Write-path tuning (pending/dedup thresholds).
    pub memory: MemoryConfig,
    /// Consolidation job tuning (periodic summarization + dedup).
    pub consolidate: ConsolidateConfig,
    /// Recall (read path) tuning: hybrid retrieval, rerank, packing, trust.
    pub recall: RecallConfig,
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
            session: SessionConfig::default(),
            budget: BudgetConfig::default(),
            memory: MemoryConfig::default(),
            consolidate: ConsolidateConfig::default(),
            recall: RecallConfig::default(),
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
        if let Ok(v) = std::env::var("AGOS_MEMORY_RECALL_TOP_K") {
            self.recall.top_k = v.parse().unwrap_or(self.recall.top_k);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_RECALL_BUDGET_TOKENS") {
            self.recall.budget_tokens = v.parse().unwrap_or(self.recall.budget_tokens);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_RECALL_MIN_SCORE") {
            self.recall.min_score = v.parse().unwrap_or(self.recall.min_score);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_RECALL_INCLUDE_EPISODIC") {
            self.recall.include_episodic = matches!(v.as_str(), "true" | "1");
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_RECALL_INCLUDE_PENDING") {
            self.recall.include_pending = matches!(v.as_str(), "true" | "1");
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_RECALL_INCLUDE_UNTRUSTED") {
            self.recall.trust_policy = if matches!(v.as_str(), "true" | "1") {
                TrustPolicy::Fenced
            } else {
                TrustPolicy::Strict
            };
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_TTL_WORKING_DAYS") {
            self.memory.ttl_working_days = v.parse().unwrap_or(self.memory.ttl_working_days);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_TTL_EPISODIC_DAYS") {
            self.memory.ttl_episodic_days = v.parse().unwrap_or(self.memory.ttl_episodic_days);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_TTL_SEMANTIC_DAYS") {
            self.memory.ttl_semantic_days = v.parse().unwrap_or(self.memory.ttl_semantic_days);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_TTL_PROCEDURAL_DAYS") {
            self.memory.ttl_procedural_days = v.parse().unwrap_or(self.memory.ttl_procedural_days);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_TTL_GRACE_DAYS") {
            self.memory.ttl_grace_days = v.parse().unwrap_or(self.memory.ttl_grace_days);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_REAPER_HOUR") {
            self.memory.reaper_hour = v.parse().unwrap_or(self.memory.reaper_hour);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_CONSOLIDATE_ENABLED") {
            self.consolidate.enabled = matches!(v.as_str(), "true" | "1");
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_CONSOLIDATE_DAY") {
            self.consolidate.day = v.parse().unwrap_or(self.consolidate.day);
        }
        if let Ok(v) = std::env::var("AGOS_MEMORY_CONSOLIDATE_HOUR") {
            self.consolidate.hour = v.parse().unwrap_or(self.consolidate.hour);
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
        if self.session.idle_minutes == 0 {
            return Err(Error::Config("session.idle_minutes must be > 0".into()));
        }
        if self.budget.max_tokens_per_session == 0 {
            return Err(Error::Config(
                "budget.max_tokens_per_session must be > 0".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.memory.pending_threshold) {
            return Err(Error::Config(
                "memory.pending_threshold must be between 0.0 and 1.0".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.memory.dedup_threshold) {
            return Err(Error::Config(
                "memory.dedup_threshold must be between 0.0 and 1.0".into(),
            ));
        }
        if self.recall.top_k == 0 {
            return Err(Error::Config("recall.top_k must be > 0".into()));
        }
        if self.recall.budget_tokens == 0 {
            return Err(Error::Config("recall.budget_tokens must be > 0".into()));
        }
        if self.recall.min_score < 0.0 || self.recall.min_score > 1.0 {
            return Err(Error::Config(
                "recall.min_score must be between 0.0 and 1.0".into(),
            ));
        }
        let split_sum = self.recall.budget_split.working
            + self.recall.budget_split.episodic
            + self.recall.budget_split.semantic
            + self.recall.budget_split.procedural;
        if !(0.95..=1.05).contains(&split_sum) {
            return Err(Error::Config(format!(
                "recall.budget_split must sum to ~1.0 (got {split_sum})"
            )));
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
            .field("recall.top_k", &self.recall.top_k)
            .field("recall.budget_tokens", &self.recall.budget_tokens)
            .field("recall.min_score", &self.recall.min_score)
            .field("recall.trust_policy", &self.recall.trust_policy)
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
