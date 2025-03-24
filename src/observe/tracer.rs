//! Tracing initialization (issue 0004).
//!
//! Spans follow `storage.write`, `recall`, `extract`, `maintain`.
//! Secrets never enter log output: `Config`'s redacted `Debug` is the only
//! sanctioned way to log configuration.

use crate::error::Result;

/// Install a global tracing subscriber using an env-filter style directive.
///
/// Precedence: `AGOS_MEMORY_LOG` env > `cfg.log_filter` > `info`.
pub fn init_tracing(log_filter: &str) -> Result<()> {
    let filter = std::env::var("AGOS_MEMORY_LOG").unwrap_or_else(|_| log_filter.to_string());
    let env_filter = tracing_subscriber::EnvFilter::try_new(&filter)
        .map_err(|e| crate::error::Error::Config(format!("invalid log filter '{filter}': {e}")))?;
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .init();
    Ok(())
}
