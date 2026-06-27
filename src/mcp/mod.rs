//! MCP server tool surface (issue 0001): `AgosServer` implements
//! `rmcp::ServerHandler` via `#[tool_router]` / `#[tool]`, exposing thin
//! adapters over the existing library calls.
//!
//! Design notes (decisions D11/D38/D39):
//! - Args structs derive `Deserialize + schemars::JsonSchema` so the
//!   `Parameters<T>` extractor auto-generates the input schema. No `Json<T>`
//!   return wrapper: result DTOs borrow nothing but their fields mix
//!   library types without `JsonSchema`, so handlers return
//!   `CallToolResponse` built by [`reply`] (one text part + structured
//!   content) directly — the macro accepts any `IntoCallToolResult`.
//! - `recall` inherits the configured Strict trust policy (D29) unless
//!   `include_untrusted` opts in — the same rule as the CLI.
//! - Transport wiring (stdio + Streamable HTTP) is issue 0008; this module
//!   is transport-agnostic and unit-tests the tools against temp-dir stores.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResponse, CallToolResult, ContentBlock, ErrorCode, ErrorData};
use rmcp::{tool, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::{Config, EmbedProvider, TrustPolicy};
use crate::storage::StoreHandle;

/// One MCP server: a store handle, its config, the configured embedder, and
/// the configured chat client.
///
/// `Clone` so transports (and axum state) can share it freely — the clone
/// is cheap (`StoreHandle` is an `Arc`, the embedder/chat are boxed behind
/// one). Both providers are injected (not built inside the tools) so tests
/// can substitute hermetic mocks (D30).
#[derive(Clone)]
pub struct AgosServer {
    store: StoreHandle,
    cfg: Arc<Config>,
    embedder: Arc<dyn crate::embed::Embedder>,
    chat: Arc<dyn crate::llm::ChatClient>,
}

impl AgosServer {
    /// Construct from an open store + config + embedder + chat client.
    ///
    /// The caller builds the embedder with
    /// `crate::embed::embedder_from_config(&cfg.embed, dim)` (dim from
    /// `store.embed_dim()`) and the chat client with
    /// `crate::llm::chat_from_config(&cfg.llm, None)`, mirroring the CLI
    /// commands. `provider = 'none'` degrades `remember`/`recall` to
    /// keyword-only (D4); `'hash'` is the hermetic test path (D30).
    pub fn new(
        store: StoreHandle,
        cfg: Config,
        embedder: Arc<dyn crate::embed::Embedder>,
        chat: Arc<dyn crate::llm::ChatClient>,
    ) -> Self {
        Self {
            store,
            cfg: Arc::new(cfg),
            embedder,
            chat,
        }
    }

    /// Borrow the shared memory API. The JSON routes (issue 0002) delegate
    /// here so the MCP and HTTP transports share one business-rules layer.
    /// Clones are cheap — `StoreHandle` is an `Arc` and the providers are
    /// boxed behind one `Arc` each.
    pub(crate) fn memory_api(&self) -> crate::api::MemoryApi {
        crate::api::MemoryApi::new(
            self.store.clone(),
            self.cfg.as_ref().clone(),
            self.embedder.clone(),
            self.chat.clone(),
        )
    }
}

/// MCP server identity: tools capability + this crate's name/version (not
/// rmcp's build environment — the default `Implementation::from_build_env()`
/// would advertise `rmcp`/its own version over the wire).
#[rmcp::tool_handler]
impl rmcp::ServerHandler for AgosServer {
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            crate::NAME,
            crate::VERSION,
        ))
    }
}

/// One text part (the fenced CLI/MCP wire format) + structured content.
///
/// `CallToolResult` is `#[non_exhaustive]`, so it is built with the
/// `success` constructor and the `structured_content` field is patched in.
fn reply(text: String, structured: serde_json::Value) -> CallToolResponse {
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(structured);
    result.into()
}

/// Turn a library error into a protocol error.
///
/// Caller-input problems surface as `INVALID_PARAMS`; everything else is
/// `INTERNAL_ERROR` with a redacted message (D21 lineage — never reflect
/// store internals or secrets over the wire). A few error kinds carry an
/// actionable, secret-free message the agent can act on and are passed
/// through deliberately:
///
/// - `BudgetExceeded`: the caller asked for more than the ceiling allows;
/// - `DbLocked`: another process holds the single-writer lock (runbook action).
fn proto(err: crate::error::Error) -> ErrorData {
    use crate::error::Error as E;
    let (code, msg) = match &err {
        E::InvalidInput(m) => (ErrorCode::INVALID_PARAMS, m.clone()),
        // Both carry an actionable, curated `Display` (leak-tested in
        // `error::tests::display_is_actionable`): a budget message names the
        // ceiling, a lock message names the holding pid.
        E::BudgetExceeded { .. } => (ErrorCode::INVALID_PARAMS, err.to_string()),
        E::DbLocked { .. } => (ErrorCode::INTERNAL_ERROR, err.to_string()),
        _ => (ErrorCode::INTERNAL_ERROR, "internal error".to_string()),
    };
    ErrorData::new(code, msg, None)
}

// ---------------------------------------------------------------------------
// Tool argument schemas
// ---------------------------------------------------------------------------

/// `remember` — one fact in, one memory out.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberArgs {
    /// The fact text to store.
    pub text: String,
    /// Memory tier (default episodic).
    pub tier: Option<String>,
    /// Memory kind (default fact).
    pub kind: Option<String>,
    /// Provenance: user/agent/tool/file/web/import (tool/web → untrusted).
    pub source_kind: Option<String>,
    /// Extraction confidence 0..1 (below threshold → pending).
    pub confidence: Option<f64>,
}

/// `recall` — hybrid retrieval against the store.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallArgs {
    /// Query text to search for.
    pub text: String,
    /// Maximum hits (default from config).
    pub k: Option<usize>,
    /// Token budget override.
    pub budget_tokens: Option<u64>,
    /// Include untrusted memories (fenced, never laundered — D29).
    pub include_untrusted: Option<bool>,
    /// Include pending memories.
    pub include_pending: Option<bool>,
    /// Include episodic-tier memories.
    pub include_episodic: Option<bool>,
}

/// `forget` — soft deprecate (default), restore, hard purge, or rollback.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForgetArgs {
    /// Public id of the memory.
    pub id: String,
    /// Action: soft | restore | hard | rollback (default soft).
    pub action: Option<String>,
    /// Rollback target version (`action = "rollback"` only).
    pub to_version: Option<i64>,
    /// Reason recorded in the forget audit ledger.
    pub reason: Option<String>,
}

/// `summarize` — on-demand summarization by id or by tier.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SummarizeArgs {
    /// Summarize a single memory by public id.
    pub id: Option<String>,
    /// Summarize all eligible memories in a tier.
    pub tier: Option<String>,
    /// Summarize every eligible memory regardless of tier cutoff.
    pub all: Option<bool>,
    /// Overwrite existing summaries.
    pub force: Option<bool>,
}

/// `explain` — provenance drill-down for one memory.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExplainArgs {
    /// Public id of the memory to explain.
    pub id: String,
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

#[tool_router]
impl AgosServer {
    /// Store one fact as a durable memory (redacted, embedded, deduped).
    #[tool(
        name = "remember",
        annotations(
            title = "Remember a fact",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResponse, ErrorData> {
        if args.text.trim().is_empty() {
            return Err(ErrorData::invalid_params("text must not be empty", None));
        }
        let tier = args.tier.as_deref().unwrap_or("episodic");
        let kind = args.kind.as_deref().unwrap_or("fact");
        let source_kind = args.source_kind.as_deref().unwrap_or("agent");
        let confidence = args.confidence.unwrap_or(1.0);
        crate::memory::check_open_session_budget(&self.store, &self.cfg)
            .await
            .map_err(proto)?;
        let row = match self.cfg.embed.provider {
            EmbedProvider::None => {
                crate::memory::remember_degraded(
                    &self.store,
                    tier,
                    kind,
                    &args.text,
                    source_kind,
                    confidence,
                    crate::observe::EXTRACTOR_VERSION,
                    self.cfg.memory.pending_threshold,
                )
                .await
            }
            _ => {
                crate::memory::remember(
                    &self.store,
                    tier,
                    kind,
                    &args.text,
                    source_kind,
                    confidence,
                    &*self.embedder,
                    crate::observe::EXTRACTOR_VERSION,
                    self.cfg.memory.pending_threshold,
                    self.cfg.memory.dedup_threshold,
                )
                .await
            }
        }
        .map_err(proto)?;
        let text = format!(
            "memory: {} (tier={}, kind={}, status={})",
            row.public_id, row.tier, row.kind, row.status
        );
        let structured = serde_json::json!({
            "public_id": row.public_id,
            "tier": row.tier,
            "kind": row.kind,
            "status": row.status,
            "trust": row.trust,
        });
        Ok(reply(text, structured))
    }

    /// Run hybrid recall against the agent's memory store.
    #[tool(
        name = "recall",
        annotations(
            title = "Recall memories",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResponse, ErrorData> {
        if args.text.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "recall query text is empty",
                None,
            ));
        }
        let trust_policy = if args.include_untrusted.unwrap_or(false) {
            TrustPolicy::Fenced
        } else {
            self.cfg.recall.trust_policy.clone()
        };
        let recall_cfg = crate::config::RecallConfig {
            top_k: args.k.unwrap_or(self.cfg.recall.top_k),
            budget_tokens: args.budget_tokens.unwrap_or(self.cfg.recall.budget_tokens),
            include_episodic: args.include_episodic.unwrap_or(false)
                || self.cfg.recall.include_episodic,
            include_pending: args.include_pending.unwrap_or(false)
                || self.cfg.recall.include_pending,
            trust_policy,
            min_score: self.cfg.recall.min_score,
            weights: self.cfg.recall.weights.clone(),
            half_life: self.cfg.recall.half_life.clone(),
            budget_split: self.cfg.recall.budget_split.clone(),
        };
        let query = crate::recall::RecallQuery::new(args.text.clone(), &recall_cfg);
        let report = crate::recall::recall(&self.store, &*self.embedder, &query)
            .await
            .map_err(proto)?;
        let texts = texts_for_report(&self.store, &report).await;
        let text = crate::recall::render_report(
            &report,
            &texts,
            &args.text,
            f64::from(self.cfg.recall.min_score),
        );
        let structured = serde_json::to_value(&report)
            .map_err(|e| proto(crate::error::Error::Storage(e.to_string())))?;
        Ok(reply(text, structured))
    }

    /// Manage memory lifecycle: soft deprecate, restore, hard purge, rollback.
    #[tool(
        name = "forget",
        annotations(
            title = "Forget a memory",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn forget(
        &self,
        Parameters(args): Parameters<ForgetArgs>,
    ) -> Result<CallToolResponse, ErrorData> {
        let action = args.action.as_deref().unwrap_or("soft");
        let row = self
            .store
            .get_memory(args.id.clone())
            .await
            .map_err(proto)?
            .ok_or_else(|| {
                ErrorData::invalid_params(format!("memory {} not found", args.id), None)
            })?;
        let (text, structured) = match action {
            "soft" => {
                self.store
                    .deprecate_memory(row.id, args.reason.as_deref().or(Some("agent")))
                    .await
                    .map_err(proto)?;
                (
                    format!("deprecated: {} (restorable)", row.public_id),
                    serde_json::json!({"public_id": row.public_id, "action": "soft"}),
                )
            }
            "restore" => {
                self.store
                    .restore_memory(row.id, Some("agent"))
                    .await
                    .map_err(proto)?;
                (
                    format!("restored: {}", row.public_id),
                    serde_json::json!({"public_id": row.public_id, "action": "restore"}),
                )
            }
            "hard" => {
                self.store
                    .hard_purge_memory(row.id, Some("agent"), args.reason.as_deref())
                    .await
                    .map_err(proto)?;
                (
                    format!("purged: {} (irreversible, tombstoned)", row.public_id),
                    serde_json::json!({"public_id": row.public_id, "action": "hard"}),
                )
            }
            "rollback" => {
                let to_version = args.to_version.ok_or_else(|| {
                    ErrorData::invalid_params("rollback requires to_version", None)
                })?;
                if to_version < 1 {
                    return Err(ErrorData::invalid_params("to_version must be >= 1", None));
                }
                let rolled = self
                    .store
                    .rollback_memory(row.id, to_version, Some("agent"))
                    .await
                    .map_err(proto)?;
                (
                    format!(
                        "rolled back: {} to v{} ({})",
                        rolled.public_id, to_version, rolled.status
                    ),
                    serde_json::json!({
                        "public_id": rolled.public_id,
                        "action": "rollback",
                        "to_version": to_version,
                    }),
                )
            }
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown forget action '{other}' (soft|restore|hard|rollback)"),
                    None,
                ));
            }
        };
        Ok(reply(text, structured))
    }

    /// Summarize memories (on-demand, by id or tier).
    #[tool(
        name = "summarize",
        annotations(
            title = "Summarize memories",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn summarize(
        &self,
        Parameters(args): Parameters<SummarizeArgs>,
    ) -> Result<CallToolResponse, ErrorData> {
        // `force` re-summarizes; the tier/all paths only ever summarize
        // memories *without* a summary (their query is
        // `summary_text IS NULL`), so `force` is meaningless there — reject it
        // explicitly instead of silently ignoring it.
        if args.force == Some(true) && args.id.is_none() {
            return Err(ErrorData::invalid_params(
                "force only applies when summarizing by id (tier/all paths skip memories \
                 that already have a summary)",
                None,
            ));
        }
        let chat = &self.chat;
        if let Some(id) = args.id.as_deref() {
            let row = self
                .store
                .get_memory(id.to_string())
                .await
                .map_err(proto)?
                .ok_or_else(|| ErrorData::invalid_params(format!("memory {id} not found"), None))?;
            let report = crate::memory::summarize_by_id(
                &self.store,
                chat,
                &self.cfg,
                row.id,
                args.force.unwrap_or(false),
            )
            .await
            .map_err(proto)?;
            let text = format!(
                "summary for {}: {}",
                report.memory.public_id, report.summary_text
            );
            let structured = serde_json::json!({
                "public_id": report.memory.public_id,
                "summary_text": report.summary_text,
                "summary_tokens": report.summary_tokens,
                "quality_score": report.quality_score,
                "overwrote": report.overwrote,
            });
            return Ok(reply(text, structured));
        }
        let tier = args.tier.as_deref().unwrap_or("episodic");
        let reports = if args.all.unwrap_or(false) {
            crate::memory::summarize_all(&self.store, chat, &self.cfg)
                .await
                .map_err(proto)?
        } else {
            crate::memory::summarize_tier(&self.store, chat, &self.cfg, tier)
                .await
                .map_err(proto)?
        };
        let scope = if args.all.unwrap_or(false) {
            "all tiers".to_string()
        } else {
            format!("tier {tier}")
        };
        let text = format!("summarized {} memories in {scope}", reports.len());
        let structured = serde_json::json!({
            "scope": if args.all.unwrap_or(false) {
                serde_json::Value::Null
            } else {
                serde_json::json!(tier)
            },
            "count": reports.len(),
            "summaries": reports.iter().map(|r| serde_json::json!({
                "public_id": r.memory.public_id,
                "summary_tokens": r.summary_tokens,
                "quality_score": r.quality_score,
            })).collect::<Vec<_>>(),
        });
        Ok(reply(text, structured))
    }

    /// Drill down into one memory's provenance, lineage, and recall history.
    #[tool(
        name = "explain",
        annotations(
            title = "Explain a memory",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn explain(
        &self,
        Parameters(args): Parameters<ExplainArgs>,
    ) -> Result<CallToolResponse, ErrorData> {
        let expl = crate::recall::explain(&self.store, &self.cfg.agent_id, &args.id)
            .await
            .map_err(proto)?
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("memory {} not found (or belongs to another agent)", args.id),
                    None,
                )
            })?;
        let text = format!(
            "id: {}\nsource: {} ({})\nsupersedes: {}\nsuperseded_by: {}\nref_count: {}\nrecall injections: {}",
            expl.public_id,
            expl.source_kind,
            expl.source_ref.as_deref().unwrap_or("—"),
            expl.supersedes.as_deref().unwrap_or("—"),
            expl.superseded_by.as_deref().unwrap_or("—"),
            expl.ref_count,
            expl.recalls.len(),
        );
        let structured = serde_json::json!({
            "public_id": expl.public_id,
            "source_kind": expl.source_kind,
            "source_ref": expl.source_ref,
            "supersedes": expl.supersedes,
            "superseded_by": expl.superseded_by,
            "ref_count": expl.ref_count,
            "links": expl.links.iter().map(|(k, t)| serde_json::json!({
                "kind": k, "target": t,
            })).collect::<Vec<_>>(),
            "recalls": expl.recalls.iter().map(|(rid, rank, score, inj)| serde_json::json!({
                "recall_id": rid, "rank": rank, "score": score, "injected": inj,
            })).collect::<Vec<_>>(),
        });
        Ok(reply(text, structured))
    }

    /// Show database health: schema, memory counts, index stats.
    #[tool(
        name = "status",
        annotations(
            title = "Server status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn status(&self) -> Result<CallToolResponse, ErrorData> {
        let schema = self.store.schema_version().await.map_err(proto)?;
        let counts = self.store.memory_counts().await.map_err(proto)?;
        let (hits, misses, entries) = self.store.embeddings_cache_stats().await.map_err(proto)?;
        let mut text = format!(
            "schema: v{schema} agent: {}\nmemories:\n",
            self.cfg.agent_id
        );
        for (status, n) in &counts {
            text.push_str(&format!("  {status}: {n}\n"));
        }
        text.push_str(&format!(
            "embeddings_cache: {entries} entries ({hits} hits, {misses} misses)"
        ));
        // `memory_counts()` groups by **status** (active/deprecated/...), so the
        // field is named `status` — the same key the JSON route returns
        // (`StatusOutcome`); `tier` here was the 0001 mislabel fixed in 0002.
        let structured = serde_json::json!({
            "schema_version": schema,
            "agent_id": self.cfg.agent_id,
            "counts": counts.iter().map(|(status, n)| serde_json::json!({
                "status": status, "count": n,
            })).collect::<Vec<_>>(),
            "embeddings_cache": {"entries": entries, "hits": hits, "misses": misses},
        });
        Ok(reply(text, structured))
    }
}

/// Store-level helpers shared by the tools.
/// Fetch injected texts for the report's surviving hits (best-effort: rows
/// missing from the map stay out and `render_report` skips dropped hits).
async fn texts_for_report(
    store: &StoreHandle,
    report: &crate::recall::RecallReport,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for hit in &report.hits {
        if let Ok(Some(row)) = store.get_memory(hit.public_id.clone()).await {
            let text = row.summary_text.as_deref().unwrap_or(&row.text);
            out.insert(hit.public_id.clone(), text.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmbedProvider;

    /// Hermetic embedder for MCP tool unit tests: normalized (case- and
    /// punctuation-stripped) text maps to the same one-hot vector, so equal
    /// wording yields cosine 1.0 and any different wording is orthogonal.
    /// Mirrors `tests/common::NormEmbedder`, which a `src/` unit test cannot
    /// import; keeps the trust/min_score assertions deterministic (D30).
    struct NormEmbedder {
        dim: usize,
    }

    impl NormEmbedder {
        fn new(dim: usize) -> Self {
            Self { dim }
        }
    }

    #[async_trait::async_trait]
    impl crate::embed::Embedder for NormEmbedder {
        async fn embed(&self, texts: &[String]) -> crate::error::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let key: String = t
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .flat_map(|c| c.to_lowercase())
                        .collect();
                    let idx = usize::from_str_radix(&crate::util::sha256_hex(&key)[..12], 16)
                        .unwrap_or(0)
                        % self.dim;
                    let mut v = vec![0.0f32; self.dim];
                    v[idx] = 1.0;
                    v
                })
                .collect())
        }

        fn model(&self) -> &str {
            "norm-mock"
        }

        fn dim(&self) -> usize {
            self.dim
        }
    }

    async fn test_server() -> (AgosServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            db_path: dir.path().join("mcp.db"),
            ..Config::default()
        };
        cfg.embed.provider = EmbedProvider::Hash;
        let store = StoreHandle::open(&cfg, 2).await.unwrap();
        let dim = store.embed_dim().await.unwrap();
        let embedder: Arc<dyn crate::embed::Embedder> = Arc::new(NormEmbedder::new(dim));
        let chat: Arc<dyn crate::llm::ChatClient> =
            Arc::new(crate::llm::MockChat::fixed("hermetic summary"));
        (AgosServer::new(store, cfg, embedder, chat), dir)
    }

    async fn call(server: &AgosServer, name: &str, args: serde_json::Value) -> CallToolResponse {
        try_call(server, name, args)
            .await
            .expect("tool call dispatches")
    }

    /// Like [`call`] but returns the `Result` instead of panicking on error.
    async fn try_call(
        server: &AgosServer,
        name: &str,
        args: serde_json::Value,
    ) -> Result<CallToolResponse, ErrorData> {
        let val = serde_json::Value::Object(args.as_object().cloned().unwrap_or_default());
        match name {
            "remember" => {
                server
                    .remember(Parameters(serde_json::from_value(val).unwrap()))
                    .await
            }
            "recall" => {
                server
                    .recall(Parameters(serde_json::from_value(val).unwrap()))
                    .await
            }
            "forget" => {
                server
                    .forget(Parameters(serde_json::from_value(val).unwrap()))
                    .await
            }
            "summarize" => {
                server
                    .summarize(Parameters(serde_json::from_value(val).unwrap()))
                    .await
            }
            "explain" => {
                server
                    .explain(Parameters(serde_json::from_value(val).unwrap()))
                    .await
            }
            "status" => server.status().await,
            _ => panic!("unknown tool: {name}"),
        }
    }

    fn structured(resp: &CallToolResponse) -> &serde_json::Value {
        match resp {
            CallToolResponse::Complete(r) => {
                r.structured_content.as_ref().expect("structured content")
            }
            other => panic!("expected complete result, got {other:?}"),
        }
    }

    fn text_of(resp: &CallToolResponse) -> String {
        match resp {
            CallToolResponse::Complete(r) => r
                .content
                .iter()
                .filter_map(|b| match b {
                    rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
            other => panic!("expected complete result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn router_lists_all_six_tools() {
        let (_server, _dir) = test_server().await;
        let tools = AgosServer::tool_router().list_all();
        let names: Vec<String> = tools.into_iter().map(|t| t.name.to_string()).collect();
        for want in [
            "remember",
            "recall",
            "forget",
            "summarize",
            "explain",
            "status",
        ] {
            assert!(
                names.contains(&want.to_string()),
                "missing {want}: {names:?}"
            );
        }
        assert_eq!(names.len(), 6, "unexpected tools: {names:?}");
    }

    #[tokio::test]
    async fn remember_then_recall_roundtrip() {
        let (server, _dir) = test_server().await;
        let resp = call(
            &server,
            "remember",
            serde_json::json!({"text": "The test agent prefers dark mode."}),
        )
        .await;
        let pid = structured(&resp)["public_id"].as_str().unwrap().to_string();
        assert!(!pid.is_empty());
        assert!(text_of(&resp).contains(&pid));

        let resp = call(
            &server,
            "recall",
            serde_json::json!({
                "text": "The test agent prefers dark mode.",
                // `remember` defaults to the episodic tier (D26: episodic is
                // opt-in for recall, so the test must opt in to see it).
                "include_episodic": true,
            }),
        )
        .await;
        let report = structured(&resp).clone();
        assert_eq!(report["no_hit"], false, "{report}");
        let ids: Vec<String> = report["hits"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h["public_id"].as_str().map(String::from))
            .collect();
        assert!(ids.contains(&pid), "{report}");
    }

    #[tokio::test]
    async fn recall_rejects_empty_query() {
        let (server, _dir) = test_server().await;
        // An all-whitespace query must be rejected by the recall tool.
        let err = try_call(&server, "recall", serde_json::json!({"text": "   "}))
            .await
            .expect_err("empty query must fail");
        assert!(err.message.contains("empty"), "{err:?}");
    }

    #[tokio::test]
    async fn trust_defaults_to_strict() {
        let (server, _dir) = test_server().await;
        // Web-sourced memories land as untrusted (D29 lineage). The query is
        // the stored text verbatim, so retrieval would succeed on similarity —
        // any difference in outcome below is purely the trust policy.
        let text = "Untrusted web factoid.";
        call(
            &server,
            "remember",
            serde_json::json!({"text": text, "source_kind": "web"}),
        )
        .await;
        let strict = call(
            &server,
            "recall",
            serde_json::json!({"text": text, "include_episodic": true}),
        )
        .await;
        let report = structured(&strict);
        assert_eq!(
            report["no_hit"], true,
            "strict must hide untrusted: {report}"
        );

        let fenced = call(
            &server,
            "recall",
            serde_json::json!({"text": text, "include_episodic": true, "include_untrusted": true}),
        )
        .await;
        let report = structured(&fenced);
        assert_eq!(report["no_hit"], false, "{report}");
    }

    #[tokio::test]
    async fn get_info_reports_this_crate_not_rmcp() {
        // 0001 acceptance: the wire identity is `agos_memory::NAME`/`VERSION`,
        // not rmcp's build environment (which is what the macro default
        // `Implementation::from_build_env()` would advertise).
        let (server, _dir) = test_server().await;
        let info = rmcp::ServerHandler::get_info(&server);
        assert_eq!(info.server_info.name, crate::NAME, "{info:?}");
        assert_eq!(info.server_info.version, crate::VERSION, "{info:?}");
        assert!(
            info.capabilities.tools.is_some(),
            "tools capability must be advertised: {info:?}"
        );
    }

    #[tokio::test]
    async fn summarize_rejects_force_without_id() {
        let (server, _dir) = test_server().await;
        let err = try_call(
            &server,
            "summarize",
            serde_json::json!({"tier": "episodic", "force": true}),
        )
        .await
        .expect_err("force without id must fail");
        assert!(err.message.contains("force"), "{err:?}");
    }

    #[tokio::test]
    async fn forget_soft_restore_hard_cycle() {
        let (server, _dir) = test_server().await;
        let resp = call(
            &server,
            "remember",
            serde_json::json!({"text": "A fact slated for forgetting."}),
        )
        .await;
        let pid = structured(&resp)["public_id"].as_str().unwrap().to_string();

        call(&server, "forget", serde_json::json!({"id": pid})).await;
        let row = server.store.get_memory(pid.clone()).await.unwrap().unwrap();
        assert_eq!(row.status, "deprecated");

        call(
            &server,
            "forget",
            serde_json::json!({"id": pid, "action": "restore"}),
        )
        .await;
        let row = server.store.get_memory(pid.clone()).await.unwrap().unwrap();
        assert_eq!(row.status, "active");

        // Hard purge must refuse recall (verified-deletion mechanics, D33).
        call(
            &server,
            "forget",
            serde_json::json!({"id": pid, "action": "hard", "reason": "test"}),
        )
        .await;
        assert!(
            server
                .store
                .get_memory(pid.clone())
                .await
                .unwrap()
                .is_none()
        );

        let q = crate::recall::RecallQuery::new(
            "fact slated for forgetting".to_string(),
            &server.cfg.recall,
        );
        let report = crate::recall::recall(&server.store, &*server.embedder, &q)
            .await
            .unwrap();
        assert!(
            !report
                .hits
                .iter()
                .any(|h| h.public_id == pid && h.injected()),
            "purged memory must not be injected"
        );
    }

    #[tokio::test]
    async fn summarize_by_id_stores_a_summary() {
        let (server, _dir) = test_server().await;
        let resp = call(
            &server,
            "remember",
            serde_json::json!({"text": "Athens is the capital of Greece."}),
        )
        .await;
        let pid = structured(&resp)["public_id"].as_str().unwrap().to_string();

        let resp = call(&server, "summarize", serde_json::json!({"id": pid})).await;
        assert!(
            !structured(&resp)["summary_text"]
                .as_str()
                .unwrap()
                .is_empty(),
            "{resp:?}"
        );
        let row = server.store.get_memory(pid).await.unwrap().unwrap();
        assert!(row.summary_text.is_some());
    }

    #[tokio::test]
    async fn explain_and_status_surface_rows() {
        let (server, _dir) = test_server().await;
        let resp = call(
            &server,
            "remember",
            serde_json::json!({"text": "A fact to explain later."}),
        )
        .await;
        let pid = structured(&resp)["public_id"].as_str().unwrap().to_string();

        let resp = call(&server, "explain", serde_json::json!({"id": pid})).await;
        assert_eq!(structured(&resp)["public_id"].as_str().unwrap(), pid);

        let resp = call(&server, "status", serde_json::json!({})).await;
        let body = structured(&resp);
        let schema = body["schema_version"].as_i64().unwrap();
        assert!(schema >= 4, "schema {schema} must carry the v4 ledgers");
        // `counts` rows are grouped by memory **status**; the key must match the
        // JSON route's `StatusOutcome.counts[].status` (0002 fixed the mislabel).
        let counts = body["counts"].as_array().unwrap();
        assert!(
            !counts.is_empty(),
            "status must report count buckets: {resp:?}"
        );
        for bucket in counts {
            assert!(bucket["status"].is_string(), "bucket: {bucket:?}");
            assert!(bucket.get("tier").is_none(), "stale `tier` key: {bucket:?}");
        }
    }
}
