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

//! Token manager for Microsoft Dataverse authentication.
//!
//! Supports multiple authentication methods:
//! - **Client credentials** (OAuth2 client_id + client_secret via Azure AD)
//! - **Azure CLI** (`az account get-access-token` for local development)
//!
//! Tokens are cached and refreshed automatically before expiry (30-second buffer).

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use drasi_lib::identity::{Credentials, IdentityProvider};

/// Authentication method for acquiring Dataverse access tokens.
#[derive(Clone)]
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

/// Manual `Debug` implementation that redacts `client_secret` so it cannot
/// leak through `tracing`, panic messages, or any `{:?}` formatting.
impl std::fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientSecret {
                tenant_id,
                client_id,
                ..
            } => f
                .debug_struct("ClientSecret")
                .field("tenant_id", tenant_id)
                .field("client_id", client_id)
                .field("client_secret", &"[REDACTED]")
                .finish(),
            Self::AzureCli => f.debug_struct("AzureCli").finish(),
        }
    }
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
    ) -> Result<Self> {
        let url = url::Url::parse(environment_url)
            .with_context(|| format!("invalid Dataverse environment_url: {environment_url}"))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("environment_url is missing a host: {environment_url}"))?;
        let resource = format!("{}://{}", url.scheme(), host);
        let scope = format!("{resource}/.default");
        let token_url = format!("https://login.microsoftonline.com/{tenant_id}/oauth2/v2.0/token");

        Ok(Self {
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
        })
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
    pub fn azure_cli(environment_url: &str) -> Result<Self> {
        let url = url::Url::parse(environment_url)
            .with_context(|| format!("invalid Dataverse environment_url: {environment_url}"))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("environment_url is missing a host: {environment_url}"))?;
        let resource = format!("{}://{}", url.scheme(), host);
        let scope = format!("{resource}/.default");

        Ok(Self {
            auth_method: AuthMethod::AzureCli,
            resource,
            scope,
            token_url: String::new(),
            http_client: reqwest::Client::new(),
            cached_token: Arc::new(RwLock::new(None)),
        })
    }

    /// Create a token manager with a custom token URL (for testing).
    pub fn with_token_url(
        tenant_id: &str,
        client_id: &str,
        client_secret: &str,
        environment_url: &str,
        token_url: &str,
    ) -> Result<Self> {
        let url = url::Url::parse(environment_url)
            .with_context(|| format!("invalid Dataverse environment_url: {environment_url}"))?;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("environment_url is missing a host: {environment_url}"))?;
        let resource = format!("{}://{}", url.scheme(), host);
        let scope = format!("{resource}/.default");

        Ok(Self {
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
        })
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
            // The full response body may contain tenant/client diagnostics that
            // shouldn't surface in user-facing error messages or aggregated
            // logs; keep it at debug level only.
            let body = response.text().await.unwrap_or_default();
            log::debug!("OAuth2 token error body: {body}");
            anyhow::bail!("OAuth2 token request failed with status {status}");
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
        log::debug!(
            "Acquiring token via Azure CLI for resource: {}",
            self.resource
        );

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
    async fn get_credentials(
        &self,
        _context: &drasi_lib::identity::CredentialContext,
    ) -> Result<Credentials> {
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
        )
        .expect("should create token manager");
        assert_eq!(tm.scope, "https://myorg.crm.dynamics.com/.default");
        assert!(tm.token_url.contains("tenant-123"));
        assert!(matches!(tm.auth_method, AuthMethod::ClientSecret { .. }));
    }

    #[test]
    fn auth_method_debug_redacts_client_secret() {
        // `AuthMethod` is held inside `TokenManager` and may surface in tracing
        // spans / panic messages; the secret must never appear in Debug output.
        let auth = AuthMethod::ClientSecret {
            tenant_id: "tenant-1".to_string(),
            client_id: "client-1".to_string(),
            client_secret: "super-secret-do-not-leak".to_string(),
        };
        let dbg = format!("{auth:?}");
        assert!(
            !dbg.contains("super-secret-do-not-leak"),
            "client_secret must not appear in Debug output: {dbg}"
        );
        assert!(dbg.contains("[REDACTED]"));
        assert!(dbg.contains("tenant-1"));
        assert!(dbg.contains("client-1"));
    }

    #[test]
    fn test_token_manager_azure_cli() {
        let tm = TokenManager::azure_cli("https://myorg.crm.dynamics.com").expect("should create");
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
        )
        .expect("should create");
        assert_eq!(tm.token_url, "http://localhost:8080/token");
    }

    #[tokio::test]
    async fn test_token_cache_initially_empty() {
        let tm = TokenManager::new("t", "c", "s", "https://test.crm.dynamics.com")
            .expect("should create");
        let cached = tm.cached_token.read().await;
        assert!(cached.is_none());
    }

    #[test]
    fn test_token_manager_rejects_invalid_url() {
        let result = TokenManager::new("t", "c", "s", "not-a-url");
        assert!(result.is_err());
    }

    #[test]
    fn test_azure_cli_rejects_invalid_url() {
        let result = TokenManager::azure_cli("not-a-url");
        assert!(result.is_err());
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

    #[test]
    fn test_parse_az_cli_expires_on_datetime_string() {
        // Older `az` CLI versions return local-time strings like
        // `"2030-01-15 12:30:00.000000"`. Verify we accept that format and
        // produce a non-zero remaining duration for a clearly future date.
        let result = TokenManager::parse_az_cli_expires_on("2099-01-01 00:00:00.000000");
        assert!(result.is_some(), "future datetime string should parse");
        let remaining = result.unwrap().duration_since(Instant::now());
        assert!(remaining.as_secs() > 0);
    }

    #[test]
    fn test_parse_az_cli_expires_on_past_datetime_yields_zero() {
        // A past datetime should clamp to zero remaining (not panic / underflow).
        let result = TokenManager::parse_az_cli_expires_on("2000-01-01 00:00:00.000000");
        assert!(result.is_some());
        let remaining = result.unwrap().saturating_duration_since(Instant::now());
        assert_eq!(remaining.as_secs(), 0);
    }

    #[tokio::test]
    async fn test_azure_cli_token_acquisition_fails_when_az_missing() {
        // Force the CLI lookup to a path that doesn't exist by overriding PATH.
        // This exercises the error branch of `refresh_token_azure_cli` without
        // requiring `az` to be absent on the developer's machine.
        use std::sync::Mutex;
        // A process-wide lock to keep the env mutation from racing other tests.
        static LOCK: Mutex<()> = Mutex::new(());
        // Snapshot/restore PATH inside a synchronous scope so the lock guard is
        // never held across an `.await` point (clippy::await_holding_lock).
        let original = {
            let _guard = LOCK.lock().unwrap();
            let original = std::env::var_os("PATH");
            // SAFETY: tests serialize env mutations via the LOCK above.
            unsafe {
                std::env::set_var("PATH", "/nonexistent-path-for-tests");
            }
            original
        };

        let tm = TokenManager::azure_cli("https://myorg.crm.dynamics.com")
            .expect("constructor should succeed");
        let result = tm.get_token().await;

        // Restore PATH before asserting so a panic doesn't leak state.
        {
            let _guard = LOCK.lock().unwrap();
            unsafe {
                match original {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
            }
        }

        assert!(
            result.is_err(),
            "token acquisition should fail when `az` is unreachable"
        );
        let err = format!("{:?}", result.unwrap_err());
        assert!(
            err.contains("az account get-access-token") || err.contains("Azure CLI"),
            "error should reference the Azure CLI invocation: {err}"
        );
    }

    /// The OAuth2 token endpoint can return error bodies that contain tenant
    /// or client diagnostics. Verify those are NOT folded into the surfaced
    /// error message (they're logged at debug level only).
    #[tokio::test]
    async fn oauth_error_body_is_not_in_surfaced_error() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let sensitive_marker = "DO_NOT_LEAK_TENANT_DIAGNOSTIC_12345";
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string(format!(
                    r#"{{"error":"invalid_client","error_description":"{sensitive_marker}"}}"#
                )),
            )
            .mount(&server)
            .await;

        let token_url = format!("{}/oauth2/v2.0/token", server.uri());
        let tm = TokenManager::with_token_url(
            "tenant-1",
            "client-1",
            "client-secret",
            "https://myorg.crm.dynamics.com",
            &token_url,
        )
        .expect("token manager should construct");

        let err = tm.get_token().await.expect_err("401 must surface as error");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("401"),
            "error must mention the HTTP status: {msg}"
        );
        assert!(
            !msg.contains(sensitive_marker),
            "OAuth error body must not appear in surfaced error: {msg}"
        );
    }
}
