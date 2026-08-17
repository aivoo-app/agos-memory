//! CLI dispatch. Modules hold one command each; `root` owns the clap parser.

pub mod backup;
pub mod cost;
pub mod export;
pub mod ingest;
pub mod init;
pub mod remember;
pub mod root;
pub mod serve;

pub use root::{Cli, run, scheduler_tick};
