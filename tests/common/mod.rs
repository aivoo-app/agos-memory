//! Shared helpers for the v0.2.0 write-path acceptance suites.
//!
//! Integration tests are separate crates, so everything they share lives here
//! (Cargo does not treat `tests/common/` as a test target).

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use agos_memory::config::Config;
use agos_memory::embed::Embedder;
use agos_memory::error::Result;
use agos_memory::storage::StoreHandle;
use agos_memory::util::sha256_hex;

/// Vector dimension used by every suite (the store pins `embed_dim` = 1536).
pub const DIM: usize = 1536;

/// Config pointing at `path` with otherwise default settings.
pub fn cfg(path: &std::path::Path) -> Config {
    Config {
        db_path: path.to_path_buf(),
        ..Config::default()
    }
}

/// Open a fresh store inside a temp dir; keep the guard alive for the test.
pub async fn store(name: &str) -> (StoreHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let handle = StoreHandle::open(&cfg(&dir.path().join(name)), 2)
        .await
        .unwrap();
    (handle, dir)
}

/// Embedder blind to case and punctuation: reformatted duplicates map to the
/// *same* vector, distinct wording maps to an orthogonal one. Deterministic,
/// so dedup thresholds are testable without a provider.
pub struct NormEmbedder {
    dim: usize,
}

impl NormEmbedder {
    /// Build for `dim` (use [`DIM`] to match the store's `vec0` table).
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

fn normalized_vector(text: &str, dim: usize) -> Vec<f32> {
    let key: String = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    let digest = sha256_hex(&key);
    let idx = usize::from_str_radix(&digest[..12], 16).unwrap_or(0) % dim;
    let mut v = vec![0.0f32; dim];
    v[idx] = 1.0;
    v
}

#[async_trait]
impl Embedder for NormEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| normalized_vector(t, self.dim))
            .collect())
    }

    fn model(&self) -> &str {
        "norm-mock"
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// [`NormEmbedder`] that records every payload it is asked to embed, so tests
/// can prove secrets never reach the provider.
pub struct RecordingEmbedder {
    inner: NormEmbedder,
    seen: Arc<Mutex<Vec<String>>>,
}

impl RecordingEmbedder {
    /// Build for `dim`, sharing `seen` with the caller.
    pub fn new(dim: usize, seen: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            inner: NormEmbedder::new(dim),
            seen,
        }
    }
}

#[async_trait]
impl Embedder for RecordingEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.seen.lock().unwrap().extend(texts.iter().cloned());
        self.inner.embed(texts).await
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }
}
