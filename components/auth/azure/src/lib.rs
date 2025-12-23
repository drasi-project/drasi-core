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
//! supporting various credential types like Managed Identity, Service Principal,
//! and DefaultAzureCredential.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_auth_azure::AzureIdentityAuth;
//!
//! // Use DefaultAzureCredential
//! let auth = AzureIdentityAuth::default();
//!
//! // Use Managed Identity
//! let auth = AzureIdentityAuth::managed_identity();
//!
//! // Get an access token
//! let token = auth.get_token(&["https://database.windows.net/.default"]).await?;
//! ```

use anyhow::Result;
use azure_core::auth::TokenCredential;
use azure_identity::{DefaultAzureCredential, DefaultAzureCredentialBuilder};
use log::debug;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Azure Identity authentication configuration
///
/// Supports different types of Azure credentials for authentication.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AzureIdentityAuth {
    /// Use DefaultAzureCredential (tries multiple credential types in order)
    Default,
    /// Use Managed Identity with optional client ID
    ManagedIdentity {
        /// Optional client ID for user-assigned managed identity
        client_id: Option<String>,
    },
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
    /// Create a managed identity configuration
    pub fn managed_identity() -> Self {
        Self::ManagedIdentity { client_id: None }
    }

    /// Create a managed identity configuration with a specific client ID
    pub fn managed_identity_with_client(client_id: impl Into<String>) -> Self {
        Self::ManagedIdentity {
            client_id: Some(client_id.into()),
        }
    }

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
                let credential = DefaultAzureCredential::default();
                Ok(Arc::new(credential))
            }
            Self::ManagedIdentity { client_id } => {
                debug!("Building ManagedIdentityCredential");
                let mut builder = DefaultAzureCredentialBuilder::new();

                // If a client ID is provided, configure it
                if let Some(id) = client_id {
                    debug!("Using managed identity with client ID: {}", id);
                    builder = builder.with_managed_identity_client_id(id.clone());
                }

                let credential = builder.build();
                Ok(Arc::new(credential))
            }
            Self::ServicePrincipal {
                tenant_id,
                client_id,
                client_secret,
            } => {
                debug!("Building ServicePrincipalCredential for tenant: {}", tenant_id);

                // Set environment variables for ClientSecretCredential
                std::env::set_var("AZURE_TENANT_ID", tenant_id);
                std::env::set_var("AZURE_CLIENT_ID", client_id);
                std::env::set_var("AZURE_CLIENT_SECRET", client_secret);

                let credential = DefaultAzureCredential::default();
                Ok(Arc::new(credential))
            }
        }
    }

    /// Get an access token for the specified scopes
    ///
    /// This is a convenience method for getting tokens directly.
    pub async fn get_token(&self, scopes: &[&str]) -> Result<String> {
        let credential = self.build_credential().await?;
        let token = credential.get_token(scopes).await?;
        Ok(token.token.secret().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_azure_identity_auth_constructors() {
        let default = AzureIdentityAuth::default();
        assert!(matches!(default, AzureIdentityAuth::Default));

        let managed = AzureIdentityAuth::managed_identity();
        assert!(matches!(
            managed,
            AzureIdentityAuth::ManagedIdentity { client_id: None }
        ));

        let managed_with_id = AzureIdentityAuth::managed_identity_with_client("test-client-id");
        match managed_with_id {
            AzureIdentityAuth::ManagedIdentity { client_id } => {
                assert_eq!(client_id, Some("test-client-id".to_string()));
            }
            _ => panic!("Expected ManagedIdentity"),
        }

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
