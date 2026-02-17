//! Background worker: polls `jobs`, dispatches handlers (issue 0026).
//!
//! The worker runs in the background, polling the `jobs` table for new work.
//! Each job kind has a registered handler. Unknown job kinds fail fast
// (retry → DLQ). The worker runs until `shutdown()` is called.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::error::Result;
use crate::llm::ChatClient;
use crate::storage::StoreHandle;

use super::extract::extract_session;
use super::jobs::JobRow;

/// Handler for one job kind: processes the payload, errors on failure.
pub type Handler = Arc<
    dyn Fn(
            JobRow,
            StoreHandle,
            Arc<dyn ChatClient>,
            Arc<Config>,
        ) -> futures::future::BoxFuture<'static, Result<()>>
        + Send
        + Sync,
>;

/// The worker: owns the store, a handler table, and a shutdown flag.
pub struct Worker {
    store: StoreHandle,
    llm: Arc<dyn ChatClient>,
    config: Arc<Config>,
    handlers: HashMap<String, Handler>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Worker {
    /// Build with no handlers; register via [`Worker::on`].
    pub fn new(store: StoreHandle, llm: Arc<dyn ChatClient>, config: Arc<Config>) -> Self {
        let mut worker = Self {
            store,
            llm,
            config,
            handlers: HashMap::new(),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        // Register built-in handlers.
        //
        // `maintain` carries a JSON payload `{"action":"ttl"|"consolidate"|"all"}`
        // (D35): consolidation is folded into the `maintain` kind rather than
        // adding a `consolidate` kind, which would need a schema-v5 `jobs` table
        // rebuild (the CHECK can't be altered in place). Unknown actions fail
        // fast (→ retry → DLQ) instead of silently doing nothing.
        worker.on("summarize", |_job, store, llm, config| {
            let store = store.clone();
            let llm = llm.clone();
            let config = config.clone();
            Box::pin(async move {
                super::summarize::run_summarization_job(&store, &llm, &config).await?;
                Ok(())
            })
        });
        worker.on("extract", |job, store, llm, config| {
            let store = store.clone();
            let llm = llm.clone();
            let config = config.clone();
            let payload = job.payload_json.clone();
            Box::pin(async move { run_extract_job(&store, &llm, &config, &payload).await })
        });
        worker.on("maintain", |job, store, llm, config| {
            let store = store.clone();
            let llm = llm.clone();
            let config = config.clone();
            let payload = job.payload_json.clone();
            Box::pin(async move { run_maintain_job(&store, &llm, &config, &payload).await })
        });
        worker
    }

    /// Register a handler for `kind`.
    pub fn on<F, Fut>(&mut self, kind: &str, f: F)
    where
        F: Fn(JobRow, StoreHandle, Arc<dyn ChatClient>, Arc<Config>) -> Fut + Send + Sync + 'static,
        Fut: futures::Future<Output = Result<()>> + Send + 'static,
    {
        self.handlers.insert(
            kind.to_string(),
            Arc::new(move |job, store, llm, config| Box::pin(f(job, store, llm, config)) as _),
        );
    }

    /// Signal shutdown; the run loop exits after the current job.
    pub fn shutdown(&self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Run until shutdown. One job at a time; unknown kinds fail (→ retry/DLQ).
    pub async fn run(&self) {
        let owner = format!("worker-{}", std::process::id());
        while !self.shutdown.load(std::sync::atomic::Ordering::SeqCst) {
            match super::jobs::claim_next(&self.store, &owner).await {
                Err(_) => tokio::time::sleep(super::jobs::POLL_INTERVAL).await,
                Ok(None) => tokio::time::sleep(super::jobs::POLL_INTERVAL).await,
                Ok(Some(job)) => {
                    let result = match self.handlers.get(&job.kind) {
                        Some(h) => {
                            h(
                                job.clone(),
                                self.store.clone(),
                                self.llm.clone(),
                                self.config.clone(),
                            )
                            .await
                        }
                        None => Err(crate::error::Error::InvalidInput(format!(
                            "no handler for job kind '{}'",
                            job.kind
                        ))),
                    };
                    match result {
                        Ok(()) => {
                            let _ = super::jobs::complete(&self.store, job.id).await;
                        }
                        Err(e) => {
                            let _ = super::jobs::fail(&self.store, job.id, &e.to_string()).await;
                        }
                    }
                }
            }
        }
    }
}

/// Run an `extract` job: payload is `{"session": <id>}`. A missing/empty
/// payload is a loud error — the job must not silently no-op (it would retry
/// forever). The worker's chat client is the configured one (built by the
/// caller via `chat_from_config`); the ledger is left to the embedder path.
async fn run_extract_job(
    store: &StoreHandle,
    llm: &std::sync::Arc<dyn ChatClient>,
    config: &Config,
    payload: &str,
) -> Result<()> {
    use crate::embed::embedder_from_config;

    let dim = store.embed_dim().await?;
    let embedder = embedder_from_config(&config.embed, dim);
    store.validate_embed_dim(&*embedder).await?;

    let v: serde_json::Value = if payload.trim().is_empty() {
        return Err(crate::error::Error::InvalidInput(
            "extract payload must be {\"session\": <id>}".into(),
        ));
    } else {
        serde_json::from_str(payload).map_err(|e| {
            crate::error::Error::InvalidInput(format!("extract payload is not JSON: {e}"))
        })?
    };
    let Some(sid) = v.get("session").and_then(|x| x.as_i64()) else {
        return Err(crate::error::Error::InvalidInput(
            "extract payload must be {\"session\": <id>}".into(),
        ));
    };

    extract_session(store, sid, llm.as_ref(), &*embedder, None).await?;
    Ok(())
}

/// Run a `maintain` job: payload `{"action":"ttl"|"consolidate"|"all"|"summarize"}`.
///
/// D35: consolidation is folded into `maintain` (no schema change — the `jobs`
/// CHECK can't be altered in place). The LLM is the configured one; offline
/// callers pass `MockChat` (empty `base_url`). Unknown actions fail fast
/// (→ retry → DLQ) instead of silently no-oping.
async fn run_maintain_job(
    store: &StoreHandle,
    llm: &std::sync::Arc<dyn ChatClient>,
    config: &Config,
    payload: &str,
) -> Result<()> {
    let action: String = if payload.trim().is_empty() {
        "all".to_string()
    } else {
        let v: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
            crate::error::Error::InvalidInput(format!("maintain payload is not JSON: {e}"))
        })?;
        v.get("action")
            .and_then(|x| x.as_str())
            .ok_or_else(|| {
                crate::error::Error::InvalidInput(
                    "maintain payload must be {\"action\": \"ttl\"|\"consolidate\"|\"all\"}".into(),
                )
            })?
            .to_string()
    };

    match action.as_str() {
        "ttl" => {
            super::ttl_reaper::run_ttl_reaper(store, config).await?;
        }
        "consolidate" => {
            super::consolidate::run_consolidation_job(store, llm, config).await?;
        }
        "summarize" => {
            super::summarize::run_summarization_job(store, llm, config).await?;
        }
        "all" => {
            super::ttl_reaper::run_ttl_reaper(store, config).await?;
            super::consolidate::run_consolidation_job(store, llm, config).await?;
            super::summarize::run_summarization_job(store, llm, config).await?;
        }
        other => {
            return Err(crate::error::Error::InvalidInput(format!(
                "unknown maintain action '{other}'; expected ttl/consolidate/all/summarize"
            )));
        }
    }
    Ok(())
}
