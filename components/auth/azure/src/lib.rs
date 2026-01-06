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

//! Azure Identity authentication for Drasi components
//!
//! This crate provides authentication using Azure Identity credentials,
//! supporting DefaultAzureCredential (which includes Managed Identity, Azure CLI, etc.)
//! and Service Principal authentication.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_auth_azure::AzureIdentityAuth;
//!
//! // Use DefaultAzureCredential (automatically tries Managed Identity, Azure CLI, etc.)
//! let auth = AzureIdentityAuth::default();
//!
//! // Get an access token
//! let token = auth.get_token(&["https://database.windows.net/.default"]).await?;
//! ```

use anyhow::Result;
use azure_core::auth::TokenCredential;
use azure_identity::{DefaultAzureCredential, TokenCredentialOptions};
use log::debug;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Azure Identity authentication configuration
///
/// Supports different types of Azure credentials for authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AzureIdentityAuth {
    /// Use DefaultAzureCredential (tries multiple credential types in order:
    /// Environment variables, Managed Identity, Azure CLI, Azure PowerShell, etc.)
    Default,
    /// Use Service Principal (client credentials)
    ServicePrincipal {
        /// Azure AD tenant ID
        tenant_id: String,
        /// Application (client) ID
        client_id: String,
        /// Client secret
        client_secret: String,
    },
}

impl Default for AzureIdentityAuth {
    fn default() -> Self {
        Self::Default
    }
}

impl AzureIdentityAuth {
    /// Create a service principal configuration
    pub fn service_principal(
        tenant_id: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self::ServicePrincipal {
            tenant_id: tenant_id.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// Build the Azure credential
    ///
    /// Creates the appropriate TokenCredential based on the configuration.
    pub async fn build_credential(&self) -> Result<Arc<dyn TokenCredential>> {
        match self {
            Self::Default => {
                debug!("Building DefaultAzureCredential");
                let credential = DefaultAzureCredential::create(TokenCredentialOptions::default())?;
                Ok(Arc::new(credential))
            }
            Self::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => {
                debug!("Building ServicePrincipalCredential for tenant: {tenant_id}",);

                // Set environment variables for ClientSecretCredential
                std::env::set_var("AZURE_TENANT_ID", tenant_id);
                std::env::set_var("AZURE_CLIENT_ID", client_id);
                std::env::set_var("AZURE_CLIENT_SECRET", client_secret);

                let credential = DefaultAzureCredential::create(TokenCredentialOptions::default())?;
                Ok(Arc::new(credential))
            }
        }
    }

    /// Get an access token for the specified scopes
    ///
    /// This is the primary method for getting access tokens. Provide the appropriate
    /// OAuth scope(s) for the Azure service you want to authenticate with.
    ///
    /// # Example
    /// ```rust,ignore
    /// let auth = AzureIdentityAuth::default();
    /// // For Azure Database for PostgreSQL/MySQL
    /// let token = auth.get_token(&["https://ossrdbms-aad.database.windows.net/.default"]).await?;
    /// ```
    pub async fn get_token(&self, scopes: &[&str]) -> Result<String> {
        let credential = self.build_credential().await?;
        let token = credential.get_token(scopes).await?;
        Ok(token.token.secret().to_string())
    }
}

/// Helper function to get an access token using DefaultAzureCredential
///
/// This is a convenience function that creates a DefaultAzureCredential
/// and fetches a token for the provided scope. DefaultAzureCredential tries
/// multiple credential types in order: environment variables, managed identity,
/// Azure CLI, Azure PowerShell, etc.
///
/// # Arguments
/// * `scope` - The OAuth scope to request (e.g., "https://ossrdbms-aad.database.windows.net/.default")
///
/// # Example
/// ```rust,ignore
/// use drasi_auth_azure::get_token_with_default_credential;
///
/// const POSTGRES_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";
/// let token = get_token_with_default_credential(POSTGRES_SCOPE).await?;
/// // Use token as password in connection string
/// ```
pub async fn get_token_with_default_credential(scope: &str) -> Result<String> {
    let auth = AzureIdentityAuth::default();
    auth.get_token(&[scope]).await
}

/// Helper function to get an access token using Service Principal credentials
///
/// This is a convenience function that creates a Service Principal credential
/// and fetches a token for the provided scope.
///
/// # Arguments
/// * `scope` - The OAuth scope to request (e.g., "https://ossrdbms-aad.database.windows.net/.default")
/// * `tenant_id` - Azure AD tenant ID
/// * `client_id` - Application (client) ID
/// * `client_secret` - Client secret
///
/// # Example
/// ```rust,ignore
/// use drasi_auth_azure::get_token_with_service_principal;
///
/// const POSTGRES_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";
/// let token = get_token_with_service_principal(
///     POSTGRES_SCOPE,
///     "tenant-id",
///     "client-id",
///     "client-secret"
/// ).await?;
/// // Use token as password in connection string
/// ```
pub async fn get_token_with_service_principal(
    scope: &str,
    tenant_id: impl Into<String>,
    client_id: impl Into<String>,
    client_secret: impl Into<String>,
) -> Result<String> {
    let auth = AzureIdentityAuth::service_principal(tenant_id, client_id, client_secret);
    auth.get_token(&[scope]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_azure_identity_auth_constructors() {
        let default = AzureIdentityAuth::default();
        assert!(matches!(default, AzureIdentityAuth::Default));

        let sp = AzureIdentityAuth::service_principal("tenant", "client", "secret");
        match sp {
            AzureIdentityAuth::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => {
                assert_eq!(tenant_id, "tenant");
                assert_eq!(client_id, "client");
                assert_eq!(client_secret, "secret");
            }
            _ => panic!("Expected ServicePrincipal"),
        }
    }
}
