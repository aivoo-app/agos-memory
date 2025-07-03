//! Observability: tracing init + LLM call cost ledger (issues 0004, 0011).

pub mod ledger;
pub mod tracer;

pub use tracer::init_tracing;

/// Version of the extraction prompt/schema, stamped onto every memory written
/// by the extractor so a prompt change can be told apart from a data change
/// (decision D10). Bump when the extraction contract changes.
pub const EXTRACTOR_VERSION: &str = "extract-v1";
