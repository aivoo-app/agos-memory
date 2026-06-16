//! Binary entry point: parse CLI, init tracing, dispatch, map errors to exit codes.
//!
//! POSIX shells (and tools like `head`/`grep`) close the stdout pipe early; by
//! default Rust panics on `BrokenPipe` ("failed printing to stdout"), which
//! surfaces as an ugly stack trace. Reset SIGPIPE to its default disposition so
//! the process simply exits (as `head`, `cat`, etc. do), keeping the CLI
//! pipeline-friendly.

#[cfg(unix)]
fn reset_sigpipe() {
    use libc;
    // SAFETY: libc::signal with SIGPIPE and default (0) handler is signal-
    // safe and only sets a disposition.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

use clap::Parser;

use agos_memory::cli::{Cli, run};
use agos_memory::config::Config;
use agos_memory::error::Error;

fn main() {
    let cli = Cli::parse();

    reset_sigpipe();

    let cfg = Config::load(cli.config.as_deref().map(std::path::Path::new)).unwrap_or_default();
    let _ = agos_memory::observe::init_tracing(&cfg.log_filter);

    // `serve` drives concurrent MCP sessions and transport tasks, so it needs a
    // multi-thread runtime; every other command is a one-shot current-thread
    // job (single-writer actor, D18).
    let serves = matches!(cli.command, agos_memory::cli::root::Command::Serve { .. });
    let result = if serves {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(cli))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(cli))
    };

    let code = match result {
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
