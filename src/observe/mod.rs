//! Observability: tracing init + LLM call cost ledger (issues 0004, 0011).

pub mod ledger;
pub mod tracer;

pub use tracer::init_tracing;
