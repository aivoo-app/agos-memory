//! Embedding providers (issue 0012).
//!
//! The [`Embedder`] trait abstracts where vectors come from:
//!
//! - `openai_compat`: an OpenAI-compatible `/v1/embeddings` endpoint.
//!   Default in AGOS: point it at **agos-proxy**, which adds masking,
//!   prompt caching, and cost accounting for free (decision D4).
//! - [`NoEmbedder`]: degraded keyword-only mode (FTS5); used when no
//!   provider is configured or the token ceiling is breached.
//!
//! The deterministic [`HashEmbedder`] backs tests and the offline eval
//! harness — CI never needs network or model access.

use async_trait::async_trait;

use crate::error::{Error, Result};
use crate::util::sha256_hex;

/// Produces embedding vectors for text batches.
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Embed a batch of texts. Output order matches input order.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Model identifier, as recorded in the database for pinning.
    fn model(&self) -> &str;

    /// Vector dimension produced by this provider.
    fn dim(&self) -> usize;
}

/// Deterministic hash-based embedder for tests and offline evals.
///
/// Maps the SHA-256 of the text onto a normalized unit vector of `dim`
/// dimensions. Same text always yields the same vector; different texts are
/// far apart in cosine terms with overwhelming probability at reasonable dims.
pub struct HashEmbedder {
    model: String,
    dim: usize,
}

impl HashEmbedder {
    /// A hash embedder with the given vector dimension.
    pub fn new(dim: usize) -> Self {
        Self {
            model: format!("hash-mock-dim{dim}"),
            dim,
        }
    }
}

#[async_trait]
impl Embedder for HashEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| hash_to_unit_vector(t, self.dim))
            .collect())
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// Degraded provider: refuses to embed. Recall falls back to FTS5 keyword search.
pub struct NoEmbedder;

#[async_trait]
impl Embedder for NoEmbedder {
    async fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(Error::Embedder(
            "no embedding provider configured (provider = none); \
             recall runs in keyword-only degraded mode"
                .into(),
        ))
    }

    fn model(&self) -> &str {
        "none"
    }

    fn dim(&self) -> usize {
        0
    }
}

/// Map text onto a deterministic pseudo-random unit vector.
///
/// Uses a character-bigram accumulation model so that texts sharing many
/// character bigrams produce more similar vectors. This gives the eval harness
/// and tests a usable similarity signal without depending on a real embedding
/// model.
pub fn hash_to_unit_vector(text: &str, dim: usize) -> Vec<f32> {
    // Accumulate bigram contributions into `dim` buckets.
    let mut buckets = vec![0.0f32; dim];
    let bytes = text.as_bytes();
    for window in bytes.windows(2) {
        // Hash the bigram to a bucket index.
        let h: u64 = ((window[0] as u64) << 8) | (window[1] as u64);
        let idx = (h % dim as u64) as usize;
        // Signed contribution: use the high bits for sign, low bits for magnitude.
        let sign: f32 = if (h >> 4) & 1 == 0 { 1.0 } else { -1.0 };
        let mag: f32 = ((h & 0xFF) as f32) / 255.0;
        buckets[idx] += sign * mag;
    }
    // Also seed each dimension with a deterministic pseudo-random value derived
    // from the full-text hash so that completely different texts are still
    // orthogonal in expectation.
    let seed_hex = sha256_hex(text);
    let mut state = u64::from_le_bytes(
        hex::decode(&seed_hex[..16])
            .expect("hex prefix is valid")
            .try_into()
            .expect("8 bytes"),
    );
    for (_i, slot) in buckets.iter_mut().enumerate() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let base: f32 = ((state % 2_000_001) as f32 - 1_000_000.0) / 1_000_000.0;
        // Blend the bigram signal (if any) with the random baseline.
        *slot = *slot * 0.7 + base * 0.3;
    }

    let norm = buckets.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        buckets.iter().map(|x| x / norm).collect()
    } else {
        buckets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_embedder_is_deterministic_and_normalized() {
        let e = HashEmbedder::new(64);
        let v = e.embed(&["hello world".into()]).await.unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), 64);
        let norm: f32 = v[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4);

        let v2 = e.embed(&["hello world".into()]).await.unwrap();
        assert_eq!(v, v2);
        assert_eq!(e.model(), "hash-mock-dim64");
    }

    #[tokio::test]
    async fn similar_texts_closer_than_different() {
        let e = HashEmbedder::new(128);
        let vs = e
            .embed(&[
                "user prefers dark mode".into(),
                "user prefers dark mode theme".into(),
                "quarterly revenue report".into(),
            ])
            .await
            .unwrap();
        let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        let near = dot(&vs[0], &vs[1]);
        let far = dot(&vs[0], &vs[2]);
        assert!(near > far, "near {near} should exceed far {far}");
    }

    #[tokio::test]
    async fn no_embedder_refuses() {
        let e = NoEmbedder;
        assert!(e.embed(&["x".into()]).await.is_err());
        assert_eq!(e.model(), "none");
        assert_eq!(e.dim(), 0);
    }
}
