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

//! Configuration types for the Qdrant reaction.

use serde::{Deserialize, Serialize};

/// Top-level configuration for the Qdrant reaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QdrantReactionConfig {
    /// Qdrant connection configuration
    pub qdrant: QdrantConfig,

    /// Embedding service configuration
    pub embedding: EmbeddingConfig,

    /// Document generation configuration
    pub document: DocumentConfig,

    /// Batch processing configuration
    #[serde(default)]
    pub batch: BatchConfig,

    /// Retry configuration for transient failures
    #[serde(default)]
    pub retry: RetryConfig,
}

/// Configuration for connecting to Qdrant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QdrantConfig {
    /// Qdrant gRPC endpoint (e.g., "http://localhost:6334")
    pub endpoint: String,

    /// Optional API key for authenticated access
    #[serde(default)]
    pub api_key: Option<String>,

    /// Name of the collection to store vectors in
    pub collection_name: String,

    /// Whether to create the collection if it doesn't exist (default: true)
    #[serde(default = "default_create_collection")]
    pub create_collection: bool,
}

fn default_create_collection() -> bool {
    true
}

/// Configuration for the embedding service.
///
/// This is a tagged enum where each variant contains only the fields relevant
/// to that embedding provider. This provides compile-time enforcement of
/// required fields per provider.
///
/// # JSON Format
///
/// ```json
/// // Mock embedder
/// {
///   "service_type": "mock",
///   "dimensions": 1536
/// }
///
/// // Azure OpenAI embedder
/// {
///   "service_type": "azure_openai",
///   "endpoint": "https://myresource.openai.azure.com",
///   "model": "text-embedding-3-large",
///   "api_key": "your-key",
///   "dimensions": 3072,
///   "api_version": "2024-02-01"
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "service_type", rename_all = "snake_case")]
pub enum EmbeddingConfig {
    /// Mock embedding service using SHA-256 (for testing)
    Mock {
        /// Embedding dimensions
        #[serde(default = "default_dimensions")]
        dimensions: usize,
    },

    /// Azure OpenAI embedding service
    AzureOpenai {
        /// Azure OpenAI endpoint
        /// e.g., "https://myresource.openai.azure.com"
        endpoint: String,

        /// Model/deployment name
        model: String,

        /// API key
        api_key: String,

        /// Embedding dimensions (default: 3072 for text-embedding-3-large)
        #[serde(default = "default_dimensions")]
        dimensions: usize,

        /// API version for Azure OpenAI (default: "2024-02-01")
        #[serde(default = "default_api_version")]
        api_version: String,
    },
}

fn default_dimensions() -> usize {
    3072
}

fn default_api_version() -> String {
    "2024-02-01".to_string()
}

impl EmbeddingConfig {
    /// Returns the embedding dimensions for this configuration.
    pub fn dimensions(&self) -> usize {
        match self {
            EmbeddingConfig::Mock { dimensions } => *dimensions,
            EmbeddingConfig::AzureOpenai { dimensions, .. } => *dimensions,
        }
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        EmbeddingConfig::Mock {
            dimensions: default_dimensions(),
        }
    }
}

/// Configuration for document generation from query results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentConfig {
    /// Handlebars template for generating document content to embed.
    /// Available variables: all fields from the query result data.
    /// Example: "Product: {{name}}. Description: {{description}}"
    pub document_template: String,

    /// Optional Handlebars template for generating document title.
    /// Example: "{{name}}"
    #[serde(default)]
    pub title_template: Option<String>,

    /// Field path to extract the unique key from query result data.
    /// Supports nested paths using dots (e.g., "after.id").
    /// This key is used to generate deterministic point IDs.
    pub key_field: String,

    /// Optional metadata fields to include in the Qdrant point payload.
    /// If not specified, all fields from the query result are included.
    #[serde(default)]
    pub metadata_fields: Option<Vec<String>>,
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            document_template: "{{content}}".to_string(),
            title_template: None,
            key_field: "id".to_string(),
            metadata_fields: None,
        }
    }
}

/// Configuration for batch processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchConfig {
    /// Maximum number of documents per embedding API call.
    /// Smaller values reduce memory usage but increase API calls.
    /// Default: 50
    #[serde(default = "default_embedding_batch_size")]
    pub embedding_batch_size: usize,

    /// Maximum number of points per Qdrant upsert call.
    /// Larger values improve throughput but increase memory usage.
    /// Default: 100
    #[serde(default = "default_upsert_batch_size")]
    pub upsert_batch_size: usize,
}

fn default_embedding_batch_size() -> usize {
    50
}

fn default_upsert_batch_size() -> usize {
    100
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            embedding_batch_size: default_embedding_batch_size(),
            upsert_batch_size: default_upsert_batch_size(),
        }
    }
}

/// Configuration for retry behavior on transient failures.
///
/// Uses exponential backoff with jitter to handle transient errors
/// from Qdrant and embedding services.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum number of retry attempts before failing.
    /// Default: 3
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Initial delay between retries in milliseconds.
    /// Default: 100
    #[serde(default = "default_initial_delay_ms")]
    pub initial_delay_ms: u64,

    /// Maximum delay between retries in milliseconds.
    /// Exponential backoff is capped at this value.
    /// Default: 5000
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_max_retries() -> u32 {
    3
}

fn default_initial_delay_ms() -> u64 {
    100
}

fn default_max_delay_ms() -> u64 {
    5000
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            initial_delay_ms: default_initial_delay_ms(),
            max_delay_ms: default_max_delay_ms(),
        }
    }
}

impl QdrantReactionConfig {
    /// Validates the configuration, returning an error if any required fields are missing.
    pub fn validate(&self) -> anyhow::Result<()> {
        // Validate Qdrant config
        if self.qdrant.endpoint.is_empty() {
            anyhow::bail!("Qdrant endpoint is required");
        }
        if self.qdrant.collection_name.is_empty() {
            anyhow::bail!("Qdrant collection name is required");
        }

        // Validate embedding config
        if self.embedding.dimensions() == 0 {
            anyhow::bail!("Embedding dimensions must be greater than 0");
        }

        // Validate document config
        if self.document.document_template.is_empty() {
            anyhow::bail!("Document template is required");
        }
        if self.document.key_field.is_empty() {
            anyhow::bail!("Key field is required");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_mock_config() {
        let config = QdrantReactionConfig {
            qdrant: QdrantConfig {
                endpoint: "http://localhost:6334".to_string(),
                api_key: None,
                collection_name: "test_collection".to_string(),
                create_collection: true,
            },
            embedding: EmbeddingConfig::Mock { dimensions: 1536 },
            document: DocumentConfig {
                document_template: "{{content}}".to_string(),
                key_field: "id".to_string(),
                ..Default::default()
            },
            batch: BatchConfig::default(),
            retry: RetryConfig::default(),
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_valid_azure_openai_config() {
        let config = QdrantReactionConfig {
            qdrant: QdrantConfig {
                endpoint: "http://localhost:6334".to_string(),
                api_key: None,
                collection_name: "test_collection".to_string(),
                create_collection: true,
            },
            embedding: EmbeddingConfig::AzureOpenai {
                endpoint: "https://myresource.openai.azure.com".to_string(),
                model: "text-embedding-3-large".to_string(),
                api_key: "secret-key".to_string(),
                dimensions: 3072,
                api_version: "2024-02-01".to_string(),
            },
            document: DocumentConfig {
                document_template: "{{content}}".to_string(),
                key_field: "id".to_string(),
                ..Default::default()
            },
            batch: BatchConfig::default(),
            retry: RetryConfig::default(),
        };

        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_missing_qdrant_endpoint() {
        let config = QdrantReactionConfig {
            qdrant: QdrantConfig {
                endpoint: "".to_string(),
                api_key: None,
                collection_name: "test_collection".to_string(),
                create_collection: true,
            },
            embedding: EmbeddingConfig::default(),
            document: DocumentConfig::default(),
            batch: BatchConfig::default(),
            retry: RetryConfig::default(),
        };

        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("endpoint is required"));
    }

    #[test]
    fn test_serde_roundtrip() {
        let config = QdrantReactionConfig {
            qdrant: QdrantConfig {
                endpoint: "http://localhost:6334".to_string(),
                api_key: Some("qdrant-key".to_string()),
                collection_name: "my_vectors".to_string(),
                create_collection: false,
            },
            embedding: EmbeddingConfig::AzureOpenai {
                endpoint: "https://test.openai.azure.com".to_string(),
                model: "embedding-model".to_string(),
                api_key: "openai-key".to_string(),
                dimensions: 1536,
                api_version: "2024-02-01".to_string(),
            },
            document: DocumentConfig {
                document_template: "Name: {{name}}, Description: {{description}}".to_string(),
                title_template: Some("{{name}}".to_string()),
                key_field: "product_id".to_string(),
                metadata_fields: Some(vec!["name".to_string(), "category".to_string()]),
            },
            batch: BatchConfig {
                embedding_batch_size: 25,
                upsert_batch_size: 50,
            },
            retry: RetryConfig {
                max_retries: 5,
                initial_delay_ms: 200,
                max_delay_ms: 10000,
            },
        };

        let json = serde_json::to_string(&config).expect("Serialization failed");
        let deserialized: QdrantReactionConfig =
            serde_json::from_str(&json).expect("Deserialization failed");

        assert_eq!(config.qdrant.endpoint, deserialized.qdrant.endpoint);
        assert_eq!(
            config.qdrant.collection_name,
            deserialized.qdrant.collection_name
        );
        assert_eq!(
            config.embedding.dimensions(),
            deserialized.embedding.dimensions()
        );
        assert_eq!(
            config.document.document_template,
            deserialized.document.document_template
        );
        assert_eq!(
            config.batch.embedding_batch_size,
            deserialized.batch.embedding_batch_size
        );
        assert_eq!(
            config.batch.upsert_batch_size,
            deserialized.batch.upsert_batch_size
        );
    }

    #[test]
    fn test_batch_config_defaults() {
        let config = BatchConfig::default();
        assert_eq!(config.embedding_batch_size, 50);
        assert_eq!(config.upsert_batch_size, 100);
    }

    #[test]
    fn test_batch_config_serde_default() {
        // Test that batch config defaults are applied when not specified in JSON
        let json = r#"{
            "qdrant": {
                "endpoint": "http://localhost:6334",
                "collection_name": "test"
            },
            "embedding": {
                "service_type": "mock",
                "dimensions": 1536
            },
            "document": {
                "document_template": "{{content}}",
                "key_field": "id"
            }
        }"#;

        let config: QdrantReactionConfig = serde_json::from_str(json).expect("Should deserialize");
        assert_eq!(config.batch.embedding_batch_size, 50);
        assert_eq!(config.batch.upsert_batch_size, 100);
    }

    #[test]
    fn test_retry_config_defaults() {
        let config = RetryConfig::default();
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_delay_ms, 100);
        assert_eq!(config.max_delay_ms, 5000);
    }

    #[test]
    fn test_retry_config_serde_default() {
        // Test that retry config defaults are applied when not specified in JSON
        let json = r#"{
            "qdrant": {
                "endpoint": "http://localhost:6334",
                "collection_name": "test"
            },
            "embedding": {
                "service_type": "mock",
                "dimensions": 1536
            },
            "document": {
                "document_template": "{{content}}",
                "key_field": "id"
            }
        }"#;

        let config: QdrantReactionConfig = serde_json::from_str(json).expect("Should deserialize");
        assert_eq!(config.retry.max_retries, 3);
        assert_eq!(config.retry.initial_delay_ms, 100);
        assert_eq!(config.retry.max_delay_ms, 5000);
    }

    #[test]
    fn test_retry_config_custom_values() {
        let json = r#"{
            "qdrant": {
                "endpoint": "http://localhost:6334",
                "collection_name": "test"
            },
            "embedding": {
                "service_type": "mock",
                "dimensions": 1536
            },
            "document": {
                "document_template": "{{content}}",
                "key_field": "id"
            },
            "retry": {
                "max_retries": 5,
                "initial_delay_ms": 200,
                "max_delay_ms": 10000
            }
        }"#;

        let config: QdrantReactionConfig = serde_json::from_str(json).expect("Should deserialize");
        assert_eq!(config.retry.max_retries, 5);
        assert_eq!(config.retry.initial_delay_ms, 200);
        assert_eq!(config.retry.max_delay_ms, 10000);
    }
}
