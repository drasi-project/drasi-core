use crate::EmbeddingService;
use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;

#[derive(Clone)]
pub enum OpenAIConfig {
    Standard {
        api_key: String,
        model: String,
    },
    Azure {
        api_key: String,
        /// The base endpoint URL (e.g., https://my-resource.openai.azure.com)
        endpoint: String,
        /// The deployment name provided in Azure Portal
        deployment: String,
        /// API Version (e.g., 2023-05-15)
        api_version: String,
    },
}

pub struct OpenAIEmbeddingService {
    client: Client,
    config: OpenAIConfig,
}

impl OpenAIEmbeddingService {
    pub fn new(config: OpenAIConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }
}

#[async_trait]
impl EmbeddingService for OpenAIEmbeddingService {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let request = match &self.config {
            OpenAIConfig::Standard { api_key, model } => self
                .client
                .post("https://api.openai.com/v1/embeddings")
                .header("Authorization", format!("Bearer {api_key}"))
                .json(&json!({
                    "input": text,
                    "model": model
                })),
            OpenAIConfig::Azure {
                api_key,
                endpoint,
                deployment,
                api_version,
            } => {
                let url = format!(
                    "{}/openai/deployments/{}/embeddings?api-version={}",
                    endpoint.trim_end_matches('/'),
                    deployment,
                    api_version
                );

                self.client
                    .post(url)
                    .header("api-key", api_key)
                    .json(&json!({
                        "input": text
                    }))
            }
        };

        let response = request
            .send()
            .await
            .context("Failed to send request to OpenAI provider")?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("OpenAI/Azure API error: {error_text}");
        }

        let body: serde_json::Value = response.json().await?;

        let embedding: Vec<f32> = body["data"][0]["embedding"]
            .as_array()
            .context("Invalid response format from API")?
            .iter()
            .map(|v| {
                v.as_f64()
                    .context("Embedding value is not a float")
                    .map(|f| f as f32)
            })
            .collect::<Result<Vec<f32>>>()?;

        Ok(embedding)
    }
}
