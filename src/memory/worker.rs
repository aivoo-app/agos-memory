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

use super::jobs::{self, JobRow, POLL_INTERVAL};
use super::summarize::run_summarization_job;

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
        // Register built-in handlers
        worker.on("summarize", |_job, store, llm, config| {
            Box::pin(async move {
                super::summarize::run_summarization_job(&store, &llm, &config).await?;
                Ok(())
            })
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
