//! CLI dispatch. Modules hold one command each; `root` owns the clap parser.

pub mod init;
pub mod root;

pub use root::{Cli, run};
