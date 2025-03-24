//! Binary entry point: parse CLI, init tracing, dispatch, map errors to exit codes.

use clap::Parser;

use agos_memory::cli::{Cli, run};
use agos_memory::config::Config;
use agos_memory::error::Error;

fn main() {
    let cli = Cli::parse();

    // Tracing first: config errors should be logged, not just printed.
    let cfg = Config::load(cli.config.as_deref().map(std::path::Path::new)).unwrap_or_default();
    let _ = agos_memory::observe::init_tracing(&cfg.log_filter);

    let code = match run(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            exit_code(&e)
        }
    };
    std::process::exit(code);
}

/// Map error taxonomy to process exit codes (stable, documented).
fn exit_code(e: &Error) -> i32 {
    match e {
        Error::Config(_) => 2,
        Error::InvalidInput(_) => 2,
        Error::SchemaTooNew { .. } => 3,
        Error::DbLocked { .. } => 4,
        Error::Storage(_) => 5,
        Error::Embedder(_) => 6,
        Error::EmbeddingMismatch { .. } => 6,
        Error::Llm(_) => 7,
        Error::BudgetExceeded { .. } => 8,
        // #[non_exhaustive] future variants default to the storage bucket.
        _ => 5,
    }
}
