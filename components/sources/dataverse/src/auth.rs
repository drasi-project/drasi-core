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

//! Token manager for Microsoft Dataverse authentication.
//!
//! Supports multiple authentication methods:
//! - **Client credentials** (OAuth2 client_id + client_secret via Azure AD)
//! - **Azure CLI** (`az account get-access-token` for local development)
//!
//! Tokens are cached and refreshed automatically before expiry (30-second buffer).

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use drasi_lib::identity::{Credentials, IdentityProvider};

/// Authentication method for acquiring Dataverse access tokens.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    /// OAuth2 client credentials flow (client_id + client_secret).
    ClientSecret {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
    /// Azure CLI token acquisition (`az account get-access-token`).
    /// Requires `az login` to have been run beforehand.
    AzureCli,
}

/// OAuth2 token response from Azure AD.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: String,
    expires_in: u64,
}

/// Response from `az account get-access-token`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AzCliTokenResponse {
    access_token: String,
    expires_on: String,
}

/// Cached OAuth2 access token with expiry tracking.
struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// Manages access tokens for Dataverse API authentication.
///
/// The token manager handles:
/// - Client credentials flow via Azure AD / Microsoft Entra ID
/// - Azure CLI token acquisition for local development
/// - Token caching to minimize authentication requests
/// - Automatic refresh when tokens are within 30 seconds of expiry
///
/// This matches the platform Dataverse source's authentication behavior
/// where `ServiceClient` handles token acquisition internally.
pub struct TokenManager {
    auth_method: AuthMethod,
    resource: String,
    scope: String,
    token_url: String,
    http_client: reqwest::Client,
    cached_token: Arc<RwLock<Option<CachedToken>>>,
}

impl TokenManager {
    /// Create a new token manager with client credentials for the given Dataverse environment.
    ///
    /// # Arguments
    ///
    /// * `tenant_id` - Azure AD tenant ID
    /// * `client_id` - Azure AD application (client) ID
    /// * `client_secret` - Client secret for the application
    /// * `environment_url` - Dataverse environment URL (e.g., `https://myorg.crm.dynamics.com`)
    pub fn new(
        tenant_id: &str,
        client_id: &str,
        client_secret: &str,
        environment_url: &str,
    ) -> Self {
        let url = url::Url::parse(environment_url).unwrap_or_else(|_| {
            url::Url::parse("https://placeholder.crm.dynamics.com")
                .expect("fallback URL should parse")
        });
        let resource = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
        let scope = format!("{resource}/.default");
        let token_url = format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token");

        Self {
            auth_method: AuthMethod::ClientSecret {
                tenant_id: tenant_id.to_string(),
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
            },
            resource,
            scope,
            token_url,
            http_client: reqwest::Client::new(),
            cached_token: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a token manager that uses Azure CLI for authentication.
    ///
    /// Requires `az login` to have been run beforehand. The token is acquired
    /// by running `az account get-access-token --resource <environment_url>`.
    ///
    /// This is the simplest option for local development and testing.
    ///
    /// # Arguments
    ///
    /// * `environment_url` - Dataverse environment URL (e.g., `https://myorg.crm.dynamics.com`)
    pub fn azure_cli(environment_url: &str) -> Self {
        let url = url::Url::parse(environment_url).unwrap_or_else(|_| {
            url::Url::parse("https://placeholder.crm.dynamics.com")
                .expect("fallback URL should parse")
        });
        let resource = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
        let scope = format!("{resource}/.default");

        Self {
            auth_method: AuthMethod::AzureCli,
            resource,
            scope,
            token_url: String::new(),
            http_client: reqwest::Client::new(),
            cached_token: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a token manager with a custom token URL (for testing).
    pub fn with_token_url(
        tenant_id: &str,
        client_id: &str,
        client_secret: &str,
        environment_url: &str,
        token_url: &str,
    ) -> Self {
        let url = url::Url::parse(environment_url).unwrap_or_else(|_| {
            url::Url::parse("https://placeholder.crm.dynamics.com")
                .expect("fallback URL should parse")
        });
        let resource = format!("{}://{}", url.scheme(), url.host_str().unwrap_or(""));
        let scope = format!("{resource}/.default");

        Self {
            auth_method: AuthMethod::ClientSecret {
                tenant_id: tenant_id.to_string(),
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
            },
            resource,
            scope,
            token_url: token_url.to_string(),
            http_client: reqwest::Client::new(),
            cached_token: Arc::new(RwLock::new(None)),
        }
    }

    /// Get a valid access token, refreshing if necessary.
    ///
    /// Returns a cached token if it is still valid (with a 30-second buffer),
    /// otherwise acquires a new token via the configured auth method.
    pub async fn get_token(&self) -> Result<String> {
        // Check if we have a valid cached token
        {
            let cached = self.cached_token.read().await;
            if let Some(ref token) = *cached {
                if Instant::now() + Duration::from_secs(30) < token.expires_at {
                    return Ok(token.access_token.clone());
                }
            }
        }

        // Acquire a new token
        match &self.auth_method {
            AuthMethod::ClientSecret { .. } => self.refresh_token_client_credentials().await,
            AuthMethod::AzureCli => self.refresh_token_azure_cli().await,
        }
    }

    /// Acquire a new OAuth2 access token via client credentials flow.
    async fn refresh_token_client_credentials(&self) -> Result<String> {
        let (client_id, client_secret) = match &self.auth_method {
            AuthMethod::ClientSecret {
                client_id,
                client_secret,
                ..
            } => (client_id.as_str(), client_secret.as_str()),
            _ => anyhow::bail!("Client credentials auth requires ClientSecret auth method"),
        };

        let params = [
            ("grant_type", "client_credentials"),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("scope", &self.scope),
        ];

        let response = self
            .http_client
            .post(&self.token_url)
            .form(&params)
            .send()
            .await
            .context("Failed to send token request to Azure AD")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("OAuth2 token request failed with status {status}: {body}");
        }

        let token_response: TokenResponse = response
            .json()
            .await
            .context("Failed to parse token response")?;

        let cached = CachedToken {
            access_token: token_response.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(token_response.expires_in),
        };

        let mut cache = self.cached_token.write().await;
        *cache = Some(cached);

        Ok(token_response.access_token)
    }

    /// Acquire an access token via Azure CLI.
    ///
    /// Runs `az account get-access-token --resource <resource>` and parses the
    /// JSON output. Requires `az login` to have been run beforehand.
    async fn refresh_token_azure_cli(&self) -> Result<String> {
        log::debug!("Acquiring token via Azure CLI for resource: {}", self.resource);

        let output = tokio::process::Command::new("az")
            .args([
                "account",
                "get-access-token",
                "--resource",
                &self.resource,
                "--output",
                "json",
            ])
            .output()
            .await
            .context(
                "Failed to run 'az account get-access-token'. \
                 Is the Azure CLI installed and have you run 'az login'?",
            )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "Azure CLI token acquisition failed (exit code {:?}): {stderr}",
                output.status.code()
            );
        }

        let cli_response: AzCliTokenResponse = serde_json::from_slice(&output.stdout)
            .context("Failed to parse Azure CLI token response")?;

        // Parse expiry from Azure CLI response (format: "2024-01-15 12:30:00.000000")
        let expires_at = Self::parse_az_cli_expires_on(&cli_response.expires_on)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        let cached = CachedToken {
            access_token: cli_response.access_token.clone(),
            expires_at,
        };

        let mut cache = self.cached_token.write().await;
        *cache = Some(cached);

        log::info!("Successfully acquired token via Azure CLI");
        Ok(cli_response.access_token)
    }

    /// Parse the `expiresOn` field from Azure CLI output to an `Instant`.
    ///
    /// Azure CLI returns timestamps like `"2024-01-15 12:30:00.000000"` in local time.
    fn parse_az_cli_expires_on(expires_on: &str) -> Option<Instant> {
        // Try parsing as a Unix timestamp first (newer az CLI versions)
        if let Ok(ts) = expires_on.parse::<i64>() {
            let now_unix = chrono::Utc::now().timestamp();
            let remaining = (ts - now_unix).max(0) as u64;
            return Some(Instant::now() + Duration::from_secs(remaining));
        }

        // Try parsing as datetime string (older az CLI format: "2024-01-15 12:30:00.000000")
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(expires_on, "%Y-%m-%d %H:%M:%S%.f")
        {
            let local_tz = chrono::Local::now().timezone();
            if let Some(local_dt) = naive.and_local_timezone(local_tz).earliest() {
                let now = chrono::Utc::now();
                let expires_utc = local_dt.with_timezone(&chrono::Utc);
                let remaining = (expires_utc - now).num_seconds().max(0) as u64;
                return Some(Instant::now() + Duration::from_secs(remaining));
            }
        }

        None
    }

    /// Get the auth method.
    pub fn auth_method(&self) -> &AuthMethod {
        &self.auth_method
    }
}

impl Clone for TokenManager {
    fn clone(&self) -> Self {
        Self {
            auth_method: self.auth_method.clone(),
            resource: self.resource.clone(),
            scope: self.scope.clone(),
            token_url: self.token_url.clone(),
            http_client: self.http_client.clone(),
            cached_token: self.cached_token.clone(), // shared cache via Arc
        }
    }
}

#[async_trait]
impl IdentityProvider for TokenManager {
    async fn get_credentials(&self) -> Result<Credentials> {
        let token = self.get_token().await?;
        Ok(Credentials::Token {
            username: "dataverse".to_string(),
            token,
        })
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_manager_creation() {
        let tm = TokenManager::new(
            "tenant-123",
            "client-456",
            "secret-789",
            "https://myorg.crm.dynamics.com",
        );
        assert_eq!(tm.scope, "https://myorg.crm.dynamics.com/.default");
        assert!(tm.token_url.contains("tenant-123"));
        assert!(matches!(tm.auth_method, AuthMethod::ClientSecret { .. }));
    }

    #[test]
    fn test_token_manager_azure_cli() {
        let tm = TokenManager::azure_cli("https://myorg.crm.dynamics.com");
        assert_eq!(tm.resource, "https://myorg.crm.dynamics.com");
        assert_eq!(tm.scope, "https://myorg.crm.dynamics.com/.default");
        assert!(matches!(tm.auth_method, AuthMethod::AzureCli));
    }

    #[test]
    fn test_token_manager_with_custom_url() {
        let tm = TokenManager::with_token_url(
            "t",
            "c",
            "s",
            "https://myorg.crm.dynamics.com",
            "http://localhost:8080/token",
        );
        assert_eq!(tm.token_url, "http://localhost:8080/token");
    }

    #[tokio::test]
    async fn test_token_cache_initially_empty() {
        let tm = TokenManager::new("t", "c", "s", "https://test.crm.dynamics.com");
        let cached = tm.cached_token.read().await;
        assert!(cached.is_none());
    }

    #[test]
    fn test_parse_az_cli_expires_on_unix() {
        // Future timestamp: 1 hour from now
        let future_ts = chrono::Utc::now().timestamp() + 3600;
        let result = TokenManager::parse_az_cli_expires_on(&future_ts.to_string());
        assert!(result.is_some());
        // Should be roughly ~3600 seconds in the future
        let remaining = result.unwrap().duration_since(Instant::now());
        assert!(remaining.as_secs() > 3500 && remaining.as_secs() <= 3600);
    }

    #[test]
    fn test_parse_az_cli_expires_on_invalid() {
        let result = TokenManager::parse_az_cli_expires_on("not-a-timestamp");
        assert!(result.is_none());
    }
}
