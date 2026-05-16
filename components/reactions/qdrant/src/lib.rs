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

//! Drasi Qdrant Reaction
//!
//! This crate provides a reaction plugin that synchronizes Drasi query results
//! to a Qdrant vector database. It generates embeddings for document content
//! and stores them as searchable vector points in Qdrant.
//!
//! # Features
//!
//! - **Azure OpenAI Embeddings**: Production-ready embedding generation using Azure OpenAI
//! - **Mock Embeddings**: Deterministic SHA-256 based embeddings for testing
//! - **Handlebars Templates**: Flexible document content generation from query results
//! - **Deterministic UUIDs**: RFC 4122 UUID v5 generation for consistent point IDs
//!
//! # Example
//!
//! ```rust,no_run
//! use drasi_reaction_qdrant::{
//!     QdrantReaction, QdrantReactionConfig, QdrantConfig,
//!     EmbeddingConfig, DocumentConfig, BatchConfig, RetryConfig, MockEmbedder,
//! };
//! use std::sync::Arc;
//!
//! fn main() -> anyhow::Result<()> {
//!     let config = QdrantReactionConfig {
//!         qdrant: QdrantConfig {
//!             endpoint: "http://localhost:6334".to_string(),
//!             api_key: None,
//!             collection_name: "my_collection".to_string(),
//!             create_collection: true,
//!         },
//!         embedding: EmbeddingConfig::Mock { dimensions: 1536 },
//!         document: DocumentConfig {
//!             document_template: "{{content}}".to_string(),
//!             key_field: "id".to_string(),
//!             ..Default::default()
//!         },
//!         batch: BatchConfig::default(),
//!         retry: RetryConfig::default(),
//!     };
//!
//!     let embedder = Arc::new(MockEmbedder::new(1536));
//!     let reaction = QdrantReaction::with_embedder(
//!         "my-qdrant-reaction",
//!         vec!["query1".to_string()],
//!         config,
//!         embedder,
//!     )?;
//!     Ok(())
//! }
//! ```

pub mod config;
pub mod document;
pub mod embedder;
pub mod qdrant;

// Re-export main types for convenience
pub use config::{
    BatchConfig, DocumentConfig, EmbeddingConfig, QdrantConfig, QdrantReactionConfig, RetryConfig,
};
pub use document::{generate_deterministic_uuid, Document, DocumentBuilder};
pub use embedder::{AzureOpenAIEmbedder, Embedder, MockEmbedder};
pub use qdrant::{ErrorHandler, QdrantReaction};
