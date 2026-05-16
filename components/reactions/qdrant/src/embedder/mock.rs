// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Mock embedder implementation using SHA-256 for deterministic testing.
//!
//! This embedder generates deterministic embeddings based on the SHA-256 hash
//! of the input text, enabling reproducible tests without external API dependencies.
//!
//! # Algorithm
//!
//! 1. Compute SHA-256 hash of the input text (32 bytes)
//! 2. For each dimension `i`:
//!    - `byte_index = (i * 4) % 32`
//!    - Construct a 32-bit seed from 4 consecutive hash bytes (wrapping)
//!    - Convert seed to a float in range [-1, 1]
//! 3. Normalize the resulting vector to unit length (L2 norm)
//!
//! This produces consistent embeddings that are:
//! - Deterministic: Same input always produces the same embedding
//! - Normalized: Unit vectors suitable for cosine similarity

use super::Embedder;
use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};

/// Mock embedder that generates deterministic embeddings using SHA-256.
///
/// # Example
///
/// ```
/// use drasi_reaction_qdrant::MockEmbedder;
/// use drasi_reaction_qdrant::Embedder;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let embedder = MockEmbedder::new(1536);
///
///     let embeddings = embedder.generate(&["Hello, world!".to_string()]).await?;
///     assert_eq!(embeddings.len(), 1);
///     assert_eq!(embeddings[0].len(), 1536);
///
///     // Embeddings are deterministic
///     let embeddings2 = embedder.generate(&["Hello, world!".to_string()]).await?;
///     assert_eq!(embeddings[0], embeddings2[0]);
///
///     Ok(())
/// }
/// ```
#[derive(Debug, Clone)]
pub struct MockEmbedder {
    dimensions: usize,
}

impl MockEmbedder {
    /// Create a new mock embedder with the specified dimensions.
    ///
    /// # Arguments
    ///
    /// * `dimensions` - The number of dimensions for generated embeddings.
    ///   Common values: 1536 (ada-002), 3072 (text-embedding-3-large)
    pub fn new(dimensions: usize) -> Self {
        Self { dimensions }
    }

    /// Generate a single embedding for the given text.
    fn generate_single(&self, text: &str) -> Vec<f32> {
        // Step 1: Compute SHA-256 hash
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();

        // Step 2: Generate raw embedding values from hash
        let mut embedding = Vec::with_capacity(self.dimensions);
        for i in 0..self.dimensions {
            let byte_index = (i * 4) % 32;

            // Construct 32-bit seed from 4 consecutive hash bytes (with wrapping)
            let seed = ((hash[byte_index] as u32) << 24)
                | ((hash[(byte_index + 1) % 32] as u32) << 16)
                | ((hash[(byte_index + 2) % 32] as u32) << 8)
                | (hash[(byte_index + 3) % 32] as u32);

            // Convert to float in range [-1, 1]
            // Match JavaScript: (unsignedSeed / 0xFFFFFFFF) * 2 - 1
            let value = (seed as f64 / u32::MAX as f64) * 2.0 - 1.0;
            embedding.push(value as f32);
        }

        // Step 3: Normalize to unit vector (L2 norm)
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for value in &mut embedding {
                *value /= magnitude;
            }
        }

        embedding
    }
}

#[async_trait]
impl Embedder for MockEmbedder {
    async fn generate(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| self.generate_single(t)).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_deterministic_embeddings() {
        let embedder = MockEmbedder::new(1536);
        let text = "Hello, world!".to_string();

        let embedding1 = embedder
            .generate(&[text.clone()])
            .await
            .expect("Failed to generate embedding");
        let embedding2 = embedder
            .generate(&[text])
            .await
            .expect("Failed to generate embedding");

        assert_eq!(
            embedding1, embedding2,
            "Same text should produce same embedding"
        );
    }

    #[tokio::test]
    async fn test_different_texts_different_embeddings() {
        let embedder = MockEmbedder::new(1536);

        let embedding1 = embedder
            .generate(&["Hello".to_string()])
            .await
            .expect("Failed to generate embedding");
        let embedding2 = embedder
            .generate(&["World".to_string()])
            .await
            .expect("Failed to generate embedding");

        assert_ne!(
            embedding1, embedding2,
            "Different texts should produce different embeddings"
        );
    }

    #[tokio::test]
    async fn test_correct_dimensions() {
        let dimensions = 3072;
        let embedder = MockEmbedder::new(dimensions);

        let embeddings = embedder
            .generate(&["test".to_string()])
            .await
            .expect("Failed to generate embedding");

        assert_eq!(embeddings.len(), 1);
        assert_eq!(embeddings[0].len(), dimensions);
        assert_eq!(embedder.dimensions(), dimensions);
    }

    #[tokio::test]
    async fn test_batch_embedding() {
        let embedder = MockEmbedder::new(1536);
        let texts = vec![
            "First text".to_string(),
            "Second text".to_string(),
            "Third text".to_string(),
        ];

        let embeddings = embedder
            .generate(&texts)
            .await
            .expect("Failed to generate embeddings");

        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), 1536);
        }
    }

    #[tokio::test]
    async fn test_normalized_embeddings() {
        let embedder = MockEmbedder::new(1536);
        let embeddings = embedder
            .generate(&["test normalization".to_string()])
            .await
            .expect("Failed to generate embedding");

        // Calculate L2 norm
        let norm: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();

        // Should be approximately 1.0 (unit vector)
        assert!(
            (norm - 1.0).abs() < 0.0001,
            "Embedding should be normalized to unit length, got norm = {norm}"
        );
    }

    #[tokio::test]
    async fn test_empty_text() {
        let embedder = MockEmbedder::new(1536);
        let embeddings = embedder
            .generate(&["".to_string()])
            .await
            .expect("Failed to generate embedding");

        assert_eq!(embeddings.len(), 1);
        assert_eq!(embeddings[0].len(), 1536);
    }

    #[tokio::test]
    async fn test_embedder_name() {
        let embedder = MockEmbedder::new(1536);
        assert_eq!(embedder.name(), "mock");
    }

    #[tokio::test]
    async fn test_unicode_text() {
        let embedder = MockEmbedder::new(1536);
        let embeddings = embedder
            .generate(&["こんにちは世界 🌍".to_string()])
            .await
            .expect("Failed to generate embedding");

        assert_eq!(embeddings.len(), 1);
        assert_eq!(embeddings[0].len(), 1536);

        // Verify normalization
        let norm: f32 = embeddings[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.0001);
    }
}
