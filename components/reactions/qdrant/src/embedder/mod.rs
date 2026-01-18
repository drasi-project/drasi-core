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

//! Embedder trait and implementations for generating text embeddings.
//!
//! This module provides:
//! - `Embedder` trait for abstracting embedding generation
//! - `MockEmbedder` for deterministic testing (SHA-256 based)
//! - `AzureOpenAIEmbedder` for production use
//!
//! The trait is designed for potential future extraction to a shared crate.

pub mod azure_openai;
pub mod mock;

pub use azure_openai::AzureOpenAIEmbedder;
pub use mock::MockEmbedder;

use anyhow::Result;
use async_trait::async_trait;

/// Trait for generating text embeddings.
///
/// This trait abstracts the embedding generation process, allowing different
/// implementations (mock, Azure OpenAI, etc.) to be used interchangeably.
///
/// # Design Notes
///
/// This trait is designed to be:
/// - **Send + Sync**: Safe for concurrent use across async tasks
/// - **Batch-oriented**: Generates embeddings for multiple texts at once for efficiency
/// - **Provider-agnostic**: Can be implemented for any embedding service
///
/// # Future Considerations
///
/// This trait is a candidate for extraction to a shared `drasi-embeddings` crate
/// to enable reuse across multiple reactions (e.g., Qdrant, Redis, pgvector).
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Generate embeddings for a batch of texts.
    ///
    /// # Arguments
    ///
    /// * `texts` - Slice of text strings to generate embeddings for
    ///
    /// # Returns
    ///
    /// A vector of embedding vectors, one for each input text.
    /// Each embedding is a vector of f32 values with length equal to `dimensions()`.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation fails (e.g., API error, network issue).
    async fn generate(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Get the dimensionality of the embeddings produced by this embedder.
    ///
    /// This is used when creating Qdrant collections to ensure the vector
    /// index is configured with the correct dimensions.
    fn dimensions(&self) -> usize;

    /// Get the name/type of this embedder for logging purposes.
    fn name(&self) -> &str;
}
