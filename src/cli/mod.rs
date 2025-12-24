//! CLI dispatch. Modules hold one command each; `root` owns the clap parser.

pub mod backup;
pub mod init;
pub mod remember;
pub mod root;

pub use root::{Cli, run, scheduler_tick};
