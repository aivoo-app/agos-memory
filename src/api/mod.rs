//! ## Transport-neutral memory API (issue 0002)
//!
//! `MemoryApi` is the single implementation of the six memory operations —
//! `remember`, `recall`, `forget`, `summarize`, `explain`, `status`. Both
//! transports call it:
//!
//! - the **MCP** tool handlers (`src/mcp`) map each `Parameters<*Input>` →
//!   `MemoryApi::*` → `(text, to_value(outcome))` for `reply`;
//! - the **JSON HTTP** routes (`src/server/json.rs`) take `Json<*Input>` →
//!   `MemoryApi::*` → `Json(outcome)`, with `{error, code}` bodies on failure.
//!
//! Because the API owns the business rules (budget guard, the `provider=none`
//! degraded path, trust policy on recall, the soft/restore/hard/rollback state
//! machine) there is exactly one code path to keep in sync — the same
//! single-vocabulary rule that 0001 applied to the MCP transport now holds for
//! JSON too.
//!
//! ### Wire compatibility
//!
//! The outcome DTOs mirror the shapes committed in 0001 *field-for-field* so
//! `cargo test` on `tests/mcp_stdio.rs` / `tests/mcp_http.rs` stays green
//! (additive-only: no committed field is removed; a couple of *new* fields are
//! added but never asserted away). One genuine bug is fixed here: the `status`
//! tool labelled its `memory_counts()` rows (grouped by **status**) as `tier`.
//! That field is corrected to `status` (changelog in 0002).

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::{Config, EmbedProvider, TrustPolicy};
use crate::error::{Error, Result};
use crate::llm::ChatClient;
use crate::memory::SummarizeReport;
use crate::recall::{RecallQuery, RecallReport, explain};
use crate::storage::StoreHandle;

/// Re-export the extractor version recorded on every memory row.
const EXTRACTOR_VERSION: &str = crate::observe::EXTRACTOR_VERSION;

// ---------------------------------------------------------------------------
// Request argument schemas (mirror the MCP tool inputs — same fields, so the
// OpenAPI spec and the MCP input schema are generated from one source).
// ---------------------------------------------------------------------------

/// `remember` — one fact in, one memory out.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RememberInput {
    /// The fact text to store.
    pub text: String,
    /// Memory tier (default episodic).
    pub tier: Option<String>,
    /// Memory kind (default fact).
    pub kind: Option<String>,
    /// Provenance: user/agent/tool/file/web/import; tool/web/import/file → untrusted.
    pub source_kind: Option<String>,
    /// Extraction confidence 0..1 (below threshold → pending).
    pub confidence: Option<f64>,
}

/// `recall` — hybrid retrieval against the store.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallInput {
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
///
/// For the JSON route the `id` arrives from the path `/api/v1/forget/{id}`; for
/// MCP it arrives in the body (same field set).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForgetInput {
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
pub struct SummarizeInput {
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
pub struct ExplainInput {
    /// Public id of the memory to explain.
    pub id: String,
}

// ---------------------------------------------------------------------------
// Outcome DTOs
// ---------------------------------------------------------------------------

/// Result of `remember`.
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct RememberOutcome {
    /// Public id of the stored memory.
    pub public_id: String,
    /// Memory tier (episodic/salient/core).
    pub tier: String,
    /// Memory kind (fact, decision, ...).
    pub kind: String,
    /// `active` | `pending`.
    pub status: String,
    /// Trust level (`trusted`/`untrusted`/`pending`).
    pub trust: String,
}

/// Result of `forget`.
///
/// `to_version` is `Some` only for `rollback`; `status` is `Some` only for
/// rollback (used to render the MCP text line) but is **never** serialized over
/// JSON — the 0001 wire shape is `{public_id, action, to_version?}`.
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct ForgetOutcome {
    /// Public id of the memory that acted on.
    pub public_id: String,
    /// `soft` | `restore` | `hard` | `rollback`.
    pub action: String,
    /// Rollback target version (`None` for soft/restore/hard).
    pub to_version: Option<i64>,
    /// Post-action row status — rollback only, skipped on the wire.
    #[serde(skip)]
    pub status: Option<String>,
}

/// Result of `summarize` by id.
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct SummarizeOutcome {
    /// Public id of the summarized memory.
    pub public_id: String,
    /// Generated summary text.
    pub summary_text: String,
    /// Token count of the summary.
    pub summary_tokens: i64,
    /// ROUGE-L F1 compression fidelity (0.0..1.0).
    pub quality_score: f64,
    /// Whether an existing summary was overwritten.
    pub overwrote: bool,
}

/// Result of `summarize` over a tier or all memories.
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct SummarizeBatchOutcome {
    /// `None` (null) for `all`, the tier string otherwise.
    pub scope: Option<String>,
    /// How many memories were summarized.
    pub count: usize,
    /// One entry per summarized memory.
    pub summaries: Vec<SummarizeEntry>,
}

/// A single entry in a [`SummarizeBatchOutcome`].
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct SummarizeEntry {
    /// Public id of the summarized memory.
    pub public_id: String,
    /// Token count of the produced summary.
    pub summary_tokens: i64,
    /// ROUGE-L F1 compression fidelity (0.0..1.0).
    pub quality_score: f64,
}

/// Result of `explain` — the `why(memory_id)` payload (0035).
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct ExplainOutcome {
    /// Public id.
    pub public_id: String,
    /// Provenance: user/agent/tool/file/web/import.
    pub source_kind: String,
    /// Provenance reference, when one was recorded.
    pub source_ref: Option<String>,
    /// Row this memory replaced, by public id.
    pub supersedes: Option<String>,
    /// Row that replaced this memory, by public id.
    pub superseded_by: Option<String>,
    /// Reference counter (read-path usage).
    pub ref_count: i64,
    /// Outgoing `memory_links` edges: `(kind, to_public_id)`.
    pub links: Vec<ExplainLink>,
    /// Recall calls that injected this memory, newest first.
    pub recalls: Vec<ExplainRecall>,
}

/// A single `memory_links` edge in an [`ExplainOutcome`].
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct ExplainLink {
    /// Edge kind (e.g. `context`, `contradicts`).
    pub kind: String,
    /// Target memory public id.
    pub target: String,
}

/// A single recall-injection row in an [`ExplainOutcome`].
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct ExplainRecall {
    /// Recall run id.
    pub recall_id: i64,
    /// Injection rank (0 = first placed).
    pub rank: i64,
    /// Final fused score.
    pub score: f64,
    /// Whether this memory was actually injected.
    pub injected: bool,
}

/// Result of `status`.
///
/// `counts` is grouped by memory **status** (a pre-existing field was
/// mislabelled `tier` in 0001; corrected here). `embeddings_cache` carries the
/// vector index cache stats.
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct StatusOutcome {
    /// Database schema version.
    pub schema_version: i64,
    /// Agent id the server is running as.
    pub agent_id: String,
    /// One entry per memory status → count.
    pub counts: Vec<StatusCount>,
    /// Vector index cache stats (entries / hits / misses).
    pub embeddings_cache: EmbeddingsCacheStats,
}

/// A `(status, count)` pair from [`StoreHandle::memory_counts`].
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct StatusCount {
    /// Memory status (`active`, `pending`, `deprecated`, ...).
    pub status: String,
    /// Row count for that status.
    pub count: i64,
}

/// `(entries, hits, misses)` from `StoreHandle::embeddings_cache_stats`.
#[derive(Debug, Clone, serde::Serialize, JsonSchema)]
pub struct EmbeddingsCacheStats {
    /// Cached embedding vectors.
    pub entries: i64,
    /// Cache hits.
    pub hits: i64,
    /// Cache misses.
    pub misses: i64,
}

/// Alias kept so both transports return the same type name for `recall`.
pub type RecallOutcome = RecallReport;

// ---------------------------------------------------------------------------
// MemoryApi: the context every transport shares.
// ---------------------------------------------------------------------------

/// Shared, transport-neutral memory context.
///
/// Built once at server startup and handed (cheaply) to both the MCP tool
/// handlers and the axum HTTP handlers. `Clone` + `Send` + `Sync` so axum state
/// can clone it per-request; the fields are all `Arc`-backed.
#[derive(Clone)]
pub struct MemoryApi {
    store: StoreHandle,
    cfg: Arc<Config>,
    embedder: Arc<dyn crate::embed::Embedder>,
    chat: Arc<dyn ChatClient>,
}

impl MemoryApi {
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
        chat: Arc<dyn ChatClient>,
    ) -> Self {
        Self {
            store,
            cfg: Arc::new(cfg),
            embedder,
            chat,
        }
    }

    /// Borrow the store handle (used by transports for health / direct reads).
    pub fn store(&self) -> &StoreHandle {
        &self.store
    }

    /// Borrow the config.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Borrow the embedder.
    pub fn embedder(&self) -> &Arc<dyn crate::embed::Embedder> {
        &self.embedder
    }

    /// Borrow the chat client.
    pub fn chat(&self) -> &Arc<dyn ChatClient> {
        &self.chat
    }

    // --- operations ----------------------------------------------------------

    /// Store one fact as a durable memory (redacted, embedded, deduped).
    pub async fn remember(&self, args: &RememberInput) -> Result<RememberOutcome> {
        if args.text.trim().is_empty() {
            return Err(Error::InvalidInput("text must not be empty".into()));
        }
        let tier = args.tier.as_deref().unwrap_or("episodic");
        let kind = args.kind.as_deref().unwrap_or("fact");
        let source_kind = args.source_kind.as_deref().unwrap_or("agent");
        let confidence = args.confidence.unwrap_or(1.0);
        crate::memory::check_open_session_budget(&self.store, &self.cfg).await?;
        let row = match self.cfg.embed.provider {
            EmbedProvider::None => {
                crate::memory::remember_degraded(
                    &self.store,
                    tier,
                    kind,
                    &args.text,
                    source_kind,
                    confidence,
                    EXTRACTOR_VERSION,
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
                    EXTRACTOR_VERSION,
                    self.cfg.memory.pending_threshold,
                    self.cfg.memory.dedup_threshold,
                )
                .await
            }
        }?;
        Ok(RememberOutcome {
            public_id: row.public_id,
            tier: row.tier,
            kind: row.kind,
            status: row.status,
            trust: row.trust,
        })
    }

    /// Run hybrid recall against the store.
    pub async fn recall(&self, args: &RecallInput) -> Result<RecallReport> {
        if args.text.trim().is_empty() {
            return Err(Error::InvalidInput("recall query text is empty".into()));
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
        let query = RecallQuery::new(args.text.clone(), &recall_cfg);
        crate::recall::recall(&self.store, &*self.embedder, &query).await
    }

    /// Manage memory lifecycle: soft deprecate, restore, hard purge, rollback.
    pub async fn forget(&self, args: &ForgetInput) -> Result<ForgetOutcome> {
        let action = args.action.as_deref().unwrap_or("soft");
        let row = self
            .store
            .get_memory(args.id.clone())
            .await?
            .ok_or_else(|| Error::MemoryNotFound {
                id: args.id.clone(),
            })?;
        match action {
            "soft" => {
                self.store
                    .deprecate_memory(row.id, args.reason.as_deref().or(Some("agent")))
                    .await?;
                Ok(ForgetOutcome {
                    public_id: row.public_id,
                    action: "soft".into(),
                    to_version: None,
                    status: None,
                })
            }
            "restore" => {
                self.store.restore_memory(row.id, Some("agent")).await?;
                Ok(ForgetOutcome {
                    public_id: row.public_id,
                    action: "restore".into(),
                    to_version: None,
                    status: None,
                })
            }
            "hard" => {
                self.store
                    .hard_purge_memory(row.id, Some("agent"), args.reason.as_deref())
                    .await?;
                Ok(ForgetOutcome {
                    public_id: row.public_id,
                    action: "hard".into(),
                    to_version: None,
                    status: None,
                })
            }
            "rollback" => {
                let to_version = args
                    .to_version
                    .ok_or_else(|| Error::InvalidInput("rollback requires to_version".into()))?;
                if to_version < 1 {
                    return Err(Error::InvalidInput("to_version must be >= 1".into()));
                }
                let rolled = self
                    .store
                    .rollback_memory(row.id, to_version, Some("agent"))
                    .await?;
                Ok(ForgetOutcome {
                    public_id: rolled.public_id,
                    action: "rollback".into(),
                    to_version: Some(to_version),
                    status: Some(rolled.status),
                })
            }
            other => Err(Error::InvalidInput(format!(
                "unknown forget action '{other}' (soft|restore|hard|rollback)"
            ))),
        }
    }

    /// Summarize memories (on-demand, by id or tier).
    pub async fn summarize(&self, args: &SummarizeInput) -> Result<SummarizeResult> {
        if args.force == Some(true) && args.id.is_none() {
            return Err(Error::InvalidInput(
                "force only applies when summarizing by id (tier/all paths skip \
                 memories that already have a summary)"
                    .into(),
            ));
        }
        let chat = &self.chat;
        if let Some(id) = args.id.as_deref() {
            let row = self
                .store
                .get_memory(id.to_string())
                .await?
                .ok_or_else(|| Error::MemoryNotFound { id: id.to_string() })?;
            let report = crate::memory::summarize_by_id(
                &self.store,
                chat,
                &self.cfg,
                row.id,
                args.force.unwrap_or(false),
            )
            .await?;
            let outcome = SummarizeOutcome {
                public_id: report.memory.public_id,
                summary_text: report.summary_text,
                summary_tokens: report.summary_tokens,
                quality_score: report.quality_score,
                overwrote: report.overwrote,
            };
            return Ok(SummarizeResult::Single(outcome));
        }
        let tier = args.tier.as_deref().unwrap_or("episodic");
        let reports: Vec<SummarizeReport> = if args.all.unwrap_or(false) {
            crate::memory::summarize_all(&self.store, chat, &self.cfg).await?
        } else {
            crate::memory::summarize_tier(&self.store, chat, &self.cfg, tier).await?
        };
        let scope = if args.all.unwrap_or(false) {
            None
        } else {
            Some(tier.to_string())
        };
        let summaries = reports
            .iter()
            .map(|r| SummarizeEntry {
                public_id: r.memory.public_id.clone(),
                summary_tokens: r.summary_tokens,
                quality_score: r.quality_score,
            })
            .collect();
        Ok(SummarizeResult::Batch(SummarizeBatchOutcome {
            scope,
            count: reports.len(),
            summaries,
        }))
    }

    /// Drill down into one memory's provenance, lineage, and recall history.
    pub async fn explain(&self, args: &ExplainInput) -> Result<Option<ExplainOutcome>> {
        let Some(expl) = explain(&self.store, &self.cfg.agent_id, &args.id).await? else {
            return Ok(None);
        };
        Ok(Some(ExplainOutcome {
            public_id: expl.public_id,
            source_kind: expl.source_kind,
            source_ref: expl.source_ref,
            supersedes: expl.supersedes,
            superseded_by: expl.superseded_by,
            ref_count: expl.ref_count,
            links: expl
                .links
                .iter()
                .map(|(k, t)| ExplainLink {
                    kind: k.clone(),
                    target: t.clone(),
                })
                .collect(),
            recalls: expl
                .recalls
                .iter()
                .map(|(rid, rank, score, inj)| ExplainRecall {
                    recall_id: *rid,
                    rank: *rank,
                    score: *score,
                    injected: *inj,
                })
                .collect(),
        }))
    }

    /// Database health: schema version, per-status counts, index cache stats.
    pub async fn status(&self) -> Result<StatusOutcome> {
        let schema = self.store.schema_version().await?;
        let raw: Vec<(String, i64)> = self.store.memory_counts().await?;
        let count_map: std::collections::HashMap<String, i64> = raw.into_iter().collect();
        // Always emit the three standard status buckets so operators and
        // dashboards see a stable schema even on a freshly-initialized store.
        let mut counts = Vec::new();
        for bucket in &["active", "deprecated", "hard-deleted"] {
            let count = count_map.get(*bucket).copied().unwrap_or(0);
            counts.push(StatusCount {
                status: bucket.to_string(),
                count,
            });
        }
        // Preserve any extra buckets the store may report (forward-compat).
        for (status, count) in count_map {
            if !["active", "deprecated", "hard-deleted"].contains(&status.as_str()) {
                counts.push(StatusCount { status, count });
            }
        }
        let (hits, misses, entries) = self.store.embeddings_cache_stats().await?;
        Ok(StatusOutcome {
            schema_version: schema,
            agent_id: self.cfg.agent_id.clone(),
            counts,
            embeddings_cache: EmbeddingsCacheStats {
                entries,
                hits,
                misses,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Branch discriminants where the shape genuinely differs by branch.
// ---------------------------------------------------------------------------

/// `summarize` returns either a single-memory outcome or a batch outcome.
///
/// `untagged` serializes [SummarizeOutcome] and [SummarizeBatchOutcome] directly
/// (no wrapper key), matching the 0001 `summarize` structured content shape.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(untagged)]
pub enum SummarizeResult {
    /// `summarize --id <id>`: one detailed summary.
    Single(SummarizeOutcome),
    /// `summarize --tier` / `--all`: a batch of light summaries.
    Batch(SummarizeBatchOutcome),
}

// ---------------------------------------------------------------------------
// Tests (hermetic; same fixture shape as the MCP module's inline tests).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Deterministic one-hot embedder: reformatted text → the same vector.
    struct HashEmbedder {
        dim: usize,
    }
    impl HashEmbedder {
        fn new(dim: usize) -> Self {
            Self { dim }
        }
    }
    #[async_trait]
    impl crate::embed::Embedder for HashEmbedder {
        async fn embed(&self, texts: &[String]) -> crate::error::Result<Vec<Vec<f32>>> {
            use crate::util::sha256_hex;
            Ok(texts
                .iter()
                .map(|t| {
                    let key: String = t
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .flat_map(|c| c.to_lowercase())
                        .collect();
                    let idx =
                        usize::from_str_radix(&sha256_hex(&key)[..12], 16).unwrap_or(0) % self.dim;
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

    async fn api() -> (MemoryApi, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config {
            db_path: dir.path().join("api.db"),
            ..Config::default()
        };
        cfg.embed.provider = EmbedProvider::Hash;
        let store = StoreHandle::open(&cfg, 2).await.unwrap();
        let dim = store.embed_dim().await.unwrap();
        let embedder: Arc<dyn crate::embed::Embedder> = Arc::new(HashEmbedder::new(dim));
        let chat: Arc<dyn ChatClient> = Arc::new(crate::llm::MockChat::fixed("hermetic summary"));
        (MemoryApi::new(store, cfg, embedder, chat), dir)
    }

    #[tokio::test]
    async fn remember_recall_forget_cycle() {
        let (api, _dir) = api().await;

        let rem = api
            .remember(&RememberInput {
                text: "Paris is the capital of France.".into(),
                tier: None,
                kind: None,
                source_kind: None,
                confidence: None,
            })
            .await
            .unwrap();
        assert!(!rem.public_id.is_empty());
        assert_eq!(rem.status, "active");
        assert_eq!(rem.trust, "trusted");

        let rec = api
            .recall(&RecallInput {
                text: "French capital".into(),
                k: None,
                budget_tokens: None,
                include_untrusted: None,
                include_pending: None,
                include_episodic: Some(true),
            })
            .await
            .unwrap();
        assert!(!rec.hits.is_empty(), "recall should hit the stored fact");

        let forgot = api
            .forget(&ForgetInput {
                id: rem.public_id.clone(),
                action: Some("soft".into()),
                to_version: None,
                reason: None,
            })
            .await
            .unwrap();
        assert_eq!(forgot.public_id, rem.public_id);
        assert_eq!(forgot.action, "soft");
        assert!(forgot.to_version.is_none());
    }

    #[tokio::test]
    async fn summarize_by_id_and_explain_and_status() {
        let (api, _dir) = api().await;
        let rem = api
            .remember(&RememberInput {
                text: "The sky is blue.".into(),
                tier: None,
                kind: None,
                source_kind: None,
                confidence: None,
            })
            .await
            .unwrap();

        match api
            .summarize(&SummarizeInput {
                id: Some(rem.public_id.clone()),
                tier: None,
                all: None,
                force: None,
            })
            .await
            .unwrap()
        {
            SummarizeResult::Single(s) => {
                assert_eq!(s.public_id, rem.public_id);
                assert!(!s.summary_text.is_empty());
            }
            SummarizeResult::Batch(_) => panic!("expected single-memory summary"),
        }

        let expl = api
            .explain(&ExplainInput {
                id: rem.public_id.clone(),
            })
            .await
            .unwrap()
            .expect("memory should exist");
        assert_eq!(expl.public_id, rem.public_id);

        let st = api.status().await.unwrap();
        assert!(st.counts.iter().any(|c| c.status == "active"));
    }

    #[test]
    fn forget_outcome_skips_status_on_wire() {
        let soft = ForgetOutcome {
            public_id: "m_1".into(),
            action: "soft".into(),
            to_version: None,
            status: None,
        };
        let json = serde_json::to_value(&soft).unwrap();
        assert_eq!(json["action"], "soft");
        assert!(
            json.get("status").is_none(),
            "status must stay off the JSON wire"
        );
    }
}
