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

//! Azure OpenAI embedder implementation.
//!
//! This module provides an embedder that uses Azure OpenAI's embedding API
//! to generate vector embeddings for text content.

use super::Embedder;
use anyhow::{Context, Result};
use async_trait::async_trait;
use log::debug;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Azure OpenAI embedder that generates embeddings using the Azure OpenAI API.
///
/// # Example
///
/// ```rust,no_run
/// use drasi_reaction_qdrant::{AzureOpenAIEmbedder, Embedder};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let embedder = AzureOpenAIEmbedder::new(
///         "https://myresource.openai.azure.com".to_string(),
///         "text-embedding-3-large".to_string(),
///         "your-api-key".to_string(),
///         3072,
///         "2024-02-01".to_string(),
///     );
///
///     let embeddings = embedder.generate(&["Hello, world!".to_string()]).await?;
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct AzureOpenAIEmbedder {
    client: Client,
    endpoint: String,
    model: String,
    api_key: String,
    dimensions: usize,
    api_version: String,
}

impl AzureOpenAIEmbedder {
    /// Create a new Azure OpenAI embedder.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - Azure OpenAI endpoint (e.g., "https://myresource.openai.azure.com")
    /// * `model` - Deployment name for the embedding model
    /// * `api_key` - Azure OpenAI API key
    /// * `dimensions` - Expected embedding dimensions
    /// * `api_version` - Azure OpenAI API version (e.g., "2024-02-01")
    pub fn new(
        endpoint: String,
        model: String,
        api_key: String,
        dimensions: usize,
        api_version: String,
    ) -> Self {
        Self {
            client: Client::new(),
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model,
            api_key,
            dimensions,
            api_version,
        }
    }

    /// Build the API URL for the embeddings endpoint.
    fn build_url(&self) -> String {
        format!(
            "{}/openai/deployments/{}/embeddings?api-version={}",
            self.endpoint, self.model, self.api_version
        )
    }
}

/// Request body for the Azure OpenAI embeddings API.
#[derive(Debug, Serialize)]
struct EmbeddingRequest {
    input: Vec<String>,
}

/// Response from the Azure OpenAI embeddings API.
#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    // model: String,
    // usage: EmbeddingUsage,
}

/// Individual embedding data from the API response.
#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

/// Error response from the Azure OpenAI API.
#[derive(Debug, Deserialize)]
struct ApiError {
    error: ApiErrorDetails,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetails {
    message: String,
    // code: Option<String>,
}

#[async_trait]
impl Embedder for AzureOpenAIEmbedder {
    async fn generate(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let url = self.build_url();
        let request_body = EmbeddingRequest {
            input: texts.to_vec(),
        };

        debug!(
            "Requesting embeddings from Azure OpenAI for {} texts",
            texts.len()
        );

        let response = self
            .client
            .post(&url)
            .header("api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .context("Failed to send request to Azure OpenAI")?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response
                .text()
                .await
                .unwrap_or_else(|_| "Unknown error".to_string());

            // Try to parse as API error
            if let Ok(api_error) = serde_json::from_str::<ApiError>(&error_text) {
                anyhow::bail!(
                    "Azure OpenAI API error ({}): {}",
                    status,
                    api_error.error.message
                );
            }

            anyhow::bail!("Azure OpenAI API error ({status}): {error_text}");
        }

        let response_body: EmbeddingResponse = response
            .json()
            .await
            .context("Failed to parse Azure OpenAI response")?;

        // Sort by index to ensure correct order
        let mut embeddings = response_body.data;
        embeddings.sort_by_key(|e| e.index);

        let result: Vec<Vec<f32>> = embeddings.into_iter().map(|e| e.embedding).collect();

        debug!("Received {} embeddings from Azure OpenAI", result.len());

        Ok(result)
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn name(&self) -> &str {
        "azure_openai"
    }
}

impl std::fmt::Debug for AzureOpenAIEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AzureOpenAIEmbedder")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("api_version", &self.api_version)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_url() {
        let embedder = AzureOpenAIEmbedder::new(
            "https://myresource.openai.azure.com".to_string(),
            "text-embedding-3-large".to_string(),
            "test-key".to_string(),
            3072,
            "2024-02-01".to_string(),
        );

        let url = embedder.build_url();
        assert_eq!(
            url,
            "https://myresource.openai.azure.com/openai/deployments/text-embedding-3-large/embeddings?api-version=2024-02-01"
        );
    }

    #[test]
    fn test_build_url_trailing_slash() {
        let embedder = AzureOpenAIEmbedder::new(
            "https://myresource.openai.azure.com/".to_string(),
            "model".to_string(),
            "key".to_string(),
            1536,
            "2024-02-01".to_string(),
        );

        let url = embedder.build_url();
        assert!(url.starts_with("https://myresource.openai.azure.com/openai/"));
        assert!(!url.contains("//openai"));
    }

    #[test]
    fn test_debug_redacts_api_key() {
        let embedder = AzureOpenAIEmbedder::new(
            "https://test.openai.azure.com".to_string(),
            "model".to_string(),
            "super-secret-key".to_string(),
            1536,
            "2024-02-01".to_string(),
        );

        let debug_str = format!("{embedder:?}");
        assert!(!debug_str.contains("super-secret-key"));
        assert!(debug_str.contains("[REDACTED]"));
    }

    #[test]
    fn test_dimensions() {
        let embedder = AzureOpenAIEmbedder::new(
            "https://test.openai.azure.com".to_string(),
            "model".to_string(),
            "key".to_string(),
            3072,
            "2024-02-01".to_string(),
        );

        assert_eq!(embedder.dimensions(), 3072);
    }

    #[test]
    fn test_name() {
        let embedder = AzureOpenAIEmbedder::new(
            "https://test.openai.azure.com".to_string(),
            "model".to_string(),
            "key".to_string(),
            1536,
            "2024-02-01".to_string(),
        );

        assert_eq!(embedder.name(), "azure_openai");
    }

    #[tokio::test]
    async fn test_empty_input() {
        let embedder = AzureOpenAIEmbedder::new(
            "https://test.openai.azure.com".to_string(),
            "model".to_string(),
            "key".to_string(),
            1536,
            "2024-02-01".to_string(),
        );

        let result = embedder.generate(&[]).await;
        assert!(result.is_ok());
        assert!(result.expect("Should succeed").is_empty());
    }
}
