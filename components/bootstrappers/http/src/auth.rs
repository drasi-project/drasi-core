// Copyright 2025 The Drasi Authors.
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

//! Authentication strategies for HTTP bootstrap requests.

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::config::{ApiKeyLocation, AuthConfig};

/// Resolved authentication that can be applied to requests.
pub enum ResolvedAuth {
    Bearer {
        token: String,
    },
    ApiKeyHeader {
        name: String,
        value: String,
    },
    ApiKeyQuery {
        name: String,
        value: String,
    },
    Basic {
        username: String,
        password: String,
    },
    OAuth2 {
        token_provider: Arc<OAuth2TokenProvider>,
    },
}

/// OAuth2 token provider with caching.
pub struct OAuth2TokenProvider {
    token_url: String,
    client_id: String,
    client_secret: String,
    scopes: Vec<String>,
    client: Client,
    cached_token: RwLock<Option<CachedToken>>,
}

#[derive(Clone)]
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct OAuth2TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[allow(dead_code)]
    #[serde(default)]
    token_type: Option<String>,
}

impl OAuth2TokenProvider {
    pub fn new(
        token_url: String,
        client_id: String,
        client_secret: String,
        scopes: Vec<String>,
        client: Client,
    ) -> Self {
        Self {
            token_url,
            client_id,
            client_secret,
            scopes,
            client,
            cached_token: RwLock::new(None),
        }
    }

    /// Get a valid access token, refreshing if expired.
    pub async fn get_token(&self) -> Result<String> {
        // Check cache first under read lock
        {
            let cache = self.cached_token.read().await;
            if let Some(ref cached) = *cache {
                if Instant::now() < cached.expires_at {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Acquire write lock and re-check to avoid stampede
        let mut cache = self.cached_token.write().await;
        if let Some(ref cached) = *cache {
            if Instant::now() < cached.expires_at {
                return Ok(cached.access_token.clone());
            }
        }

        // Token expired or not cached, fetch new one
        let token = self.fetch_token().await?;
        let access_token = token.access_token.clone();
        *cache = Some(token);

        Ok(access_token)
    }

    async fn fetch_token(&self) -> Result<CachedToken> {
        let mut form = vec![
            ("grant_type", "client_credentials".to_string()),
            ("client_id", self.client_id.clone()),
            ("client_secret", self.client_secret.clone()),
        ];

        if !self.scopes.is_empty() {
            form.push(("scope", self.scopes.join(" ")));
        }

        let response = self
            .client
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .context("Failed to request OAuth2 token")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read response".to_string());
            return Err(anyhow::anyhow!(
                "OAuth2 token request failed with status {status}: {body}"
            ));
        }

        let token_response: OAuth2TokenResponse = response
            .json()
            .await
            .context("Failed to parse OAuth2 token response")?;

        // Default to 1 hour expiry with 60-second safety margin
        let expires_in = token_response.expires_in.unwrap_or(3600);
        let expires_at = Instant::now() + Duration::from_secs(expires_in.saturating_sub(60));

        Ok(CachedToken {
            access_token: token_response.access_token,
            expires_at,
        })
    }
}

/// Resolve an AuthConfig into a ResolvedAuth by reading environment variables.
pub fn resolve_auth(config: &AuthConfig, client: &Client) -> Result<ResolvedAuth> {
    match config {
        AuthConfig::Bearer { token_env } => {
            let token = std::env::var(token_env)
                .with_context(|| format!("Environment variable '{token_env}' not set"))?;
            Ok(ResolvedAuth::Bearer { token })
        }
        AuthConfig::ApiKey {
            location,
            name,
            value_env,
        } => {
            let value = std::env::var(value_env)
                .with_context(|| format!("Environment variable '{value_env}' not set"))?;
            match location {
                ApiKeyLocation::Header => Ok(ResolvedAuth::ApiKeyHeader {
                    name: name.clone(),
                    value,
                }),
                ApiKeyLocation::Query => Ok(ResolvedAuth::ApiKeyQuery {
                    name: name.clone(),
                    value,
                }),
            }
        }
        AuthConfig::Basic {
            username_env,
            password_env,
        } => {
            let username = std::env::var(username_env)
                .with_context(|| format!("Environment variable '{username_env}' not set"))?;
            let password = match password_env {
                Some(env) => std::env::var(env)
                    .with_context(|| format!("Environment variable '{env}' not set"))?,
                None => String::new(),
            };
            Ok(ResolvedAuth::Basic { username, password })
        }
        AuthConfig::OAuth2ClientCredentials {
            token_url,
            client_id_env,
            client_secret_env,
            scopes,
        } => {
            let client_id = std::env::var(client_id_env)
                .with_context(|| format!("Environment variable '{client_id_env}' not set"))?;
            let client_secret = std::env::var(client_secret_env)
                .with_context(|| format!("Environment variable '{client_secret_env}' not set"))?;

            let provider = OAuth2TokenProvider::new(
                token_url.clone(),
                client_id,
                client_secret,
                scopes.clone(),
                client.clone(),
            );

            Ok(ResolvedAuth::OAuth2 {
                token_provider: Arc::new(provider),
            })
        }
    }
}

/// Apply resolved authentication to a request builder.
pub async fn apply_auth(
    builder: reqwest::RequestBuilder,
    auth: &ResolvedAuth,
) -> Result<reqwest::RequestBuilder> {
    match auth {
        ResolvedAuth::Bearer { token } => Ok(builder.bearer_auth(token)),
        ResolvedAuth::ApiKeyHeader { name, value } => {
            let mut headers = HeaderMap::new();
            let header_name = HeaderName::try_from(name.as_str())
                .with_context(|| format!("Invalid header name: {name}"))?;
            let header_value = HeaderValue::from_str(value)
                .with_context(|| format!("Invalid header value for {name}"))?;
            headers.insert(header_name, header_value);
            Ok(builder.headers(headers))
        }
        ResolvedAuth::ApiKeyQuery { name, value } => Ok(builder.query(&[(name, value)])),
        ResolvedAuth::Basic { username, password } => {
            Ok(builder.basic_auth(username, Some(password)))
        }
        ResolvedAuth::OAuth2 { token_provider } => {
            let token = token_provider
                .get_token()
                .await
                .context("Failed to get OAuth2 token")?;
            Ok(builder.bearer_auth(token))
        }
    }
}
