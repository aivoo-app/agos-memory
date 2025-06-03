//! Background worker: polls `jobs`, dispatches handlers (issue 0026).

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;
use crate::storage::StoreHandle;

use super::jobs::{self, JobRow, POLL_INTERVAL};

/// Handler for one job kind: processes the payload, errors on failure.
pub type Handler = Arc<
    dyn Fn(JobRow, StoreHandle) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync,
>;

/// The worker: owns the store, a handler table, and a shutdown flag.
pub struct Worker {
    store: StoreHandle,
    handlers: HashMap<String, Handler>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
}

impl Worker {
    /// Build with no handlers; register via [`Worker::on`].
    pub fn new(store: StoreHandle) -> Self {
        Self {
            store,
            handlers: HashMap::new(),
            shutdown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Register a handler for `kind`.
    pub fn on<F, Fut>(&mut self, kind: &str, f: F)
    where
        F: Fn(JobRow, StoreHandle) -> Fut + Send + Sync + 'static,
        Fut: futures::Future<Output = Result<()>> + Send + 'static,
    {
        self.handlers.insert(
            kind.to_string(),
            Arc::new(move |job, store| Box::pin(f(job, store)) as _),
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
            match jobs::claim_next(&self.store, &owner).await {
                Err(_) => tokio::time::sleep(POLL_INTERVAL).await,
                Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
                Ok(Some(job)) => {
                    let result = match self.handlers.get(&job.kind) {
                        Some(h) => h(job.clone(), self.store.clone()).await,
                        None => Err(crate::error::Error::InvalidInput(format!(
                            "no handler for job kind '{}'",
                            job.kind
                        ))),
                    };
                    match result {
                        Ok(()) => {
                            let _ = jobs::complete(&self.store, job.id).await;
                        }
                        Err(e) => {
                            let _ = jobs::fail(&self.store, job.id, &e.to_string()).await;
                        }
                    }
                }
            }
        }
    }
}
