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

use super::{Credentials, IdentityProvider};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use azure_core::credentials::TokenCredential;
use azure_identity::{
    DeveloperToolsCredential, ManagedIdentityCredential, ManagedIdentityCredentialOptions,
    UserAssignedId, WorkloadIdentityCredential,
};
use std::sync::Arc;

const DEFAULT_AZURE_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";

/// Identity provider for Azure AD authentication.
///
/// Each instance represents a single authentication method.
/// Create multiple providers if you need fallback behavior.
///
/// # Identity Name
///
/// The `identity_name` is the database identity used for authentication.
/// For Azure Database for PostgreSQL, this should be in the format:
/// `user@servername` (e.g., `myuser@myserver`).
///
/// For Azure AD authentication, use the Azure AD principal name
/// (e.g., `user@tenant.onmicrosoft.com`).
#[derive(Clone)]
pub struct AzureIdentityProvider {
    credential: Arc<dyn TokenCredential>,
    identity_name: String,
    scope: String,
}

impl AzureIdentityProvider {
    /// Create provider using system-assigned managed identity.
    ///
    /// The `identity_name` is the database identity used for authentication,
    /// typically in the format `user@tenant.onmicrosoft.com` for Azure AD.
    ///
    /// This is appropriate for production workloads running in Azure with
    /// a system-assigned managed identity.
    pub fn new(identity_name: impl Into<String>) -> Result<Self> {
        let credential = ManagedIdentityCredential::new(None)
            .map_err(|e| anyhow!("Failed to create managed identity credential: {e}"))?;

        Ok(Self {
            credential: credential as Arc<dyn TokenCredential>,
            identity_name: identity_name.into(),
            scope: DEFAULT_AZURE_SCOPE.to_string(),
        })
    }

    /// Create provider with a user-assigned managed identity client ID.
    ///
    /// This is useful for user-assigned managed identities where you need to
    /// specify which identity to use.
    pub fn with_managed_identity(
        identity_name: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Result<Self> {
        let mut options = ManagedIdentityCredentialOptions::default();
        options.user_assigned_id = Some(UserAssignedId::ClientId(client_id.into()));
        let credential = ManagedIdentityCredential::new(Some(options))
            .map_err(|e| anyhow!("Failed to create managed identity credential: {e}"))?;

        Ok(Self {
            credential: credential as Arc<dyn TokenCredential>,
            identity_name: identity_name.into(),
            scope: DEFAULT_AZURE_SCOPE.to_string(),
        })
    }

    /// Create provider using Azure default credential chain.
    ///
    /// This attempts multiple developer tool authentication methods in order:
    /// 1. **Azure CLI**: If `az login` has been run locally
    /// 2. **Azure Developer CLI**: If `azd auth login` has been run
    /// 3. **Azure PowerShell**: If `Connect-AzAccount` has been run
    ///
    /// This is recommended for local development where developers have authenticated
    /// using one of the Azure developer tools. For production deployments in Azure,
    /// use `new()` for system-assigned managed identity or `with_managed_identity()`
    /// for user-assigned managed identity.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_lib::identity::AzureIdentityProvider;
    ///
    /// // Works with az login, azd auth login, or Connect-AzAccount
    /// let provider = AzureIdentityProvider::with_default_credentials(
    ///     "myuser@tenant.onmicrosoft.com"
    /// )?;
    /// ```
    pub fn with_default_credentials(identity_name: impl Into<String>) -> Result<Self> {
        let credential = DeveloperToolsCredential::new(None)
            .map_err(|e| anyhow!("Failed to create developer tools credential: {e}"))?;

        Ok(Self {
            credential: credential as Arc<dyn TokenCredential>,
            identity_name: identity_name.into(),
            scope: DEFAULT_AZURE_SCOPE.to_string(),
        })
    }

    /// Create provider using Workload Identity for AKS.
    ///
    /// This uses environment variables injected by AKS Workload Identity:
    /// - `AZURE_CLIENT_ID`
    /// - `AZURE_TENANT_ID`
    /// - `AZURE_FEDERATED_TOKEN_FILE`
    ///
    /// This is the **recommended method for AKS deployments** with Workload Identity enabled.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_lib::identity::AzureIdentityProvider;
    ///
    /// // For AKS with Workload Identity
    /// let provider = AzureIdentityProvider::with_workload_identity(
    ///     "myidentity"  // The managed identity name
    /// )?;
    /// ```
    pub fn with_workload_identity(identity_name: impl Into<String>) -> Result<Self> {
        let credential = WorkloadIdentityCredential::new(None)
            .map_err(|e| anyhow!("Failed to create workload identity credential: {e}"))?;

        Ok(Self {
            credential: credential as Arc<dyn TokenCredential>,
            identity_name: identity_name.into(),
            scope: DEFAULT_AZURE_SCOPE.to_string(),
        })
    }

    /// Set a custom scope for token acquisition.
    ///
    /// The default scope is `https://ossrdbms-aad.database.windows.net/.default`
    /// which is appropriate for Azure Database for PostgreSQL and MySQL.
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }
}

#[async_trait]
impl IdentityProvider for AzureIdentityProvider {
    async fn get_credentials(&self) -> Result<Credentials> {
        let token_response = self
            .credential
            .get_token(&[&self.scope], None)
            .await
            .map_err(|e| anyhow!("Failed to get Azure AD token: {e}"))?;

        Ok(Credentials::Token {
            username: self.identity_name.clone(),
            token: token_response.token.secret().to_string(),
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
    fn test_with_managed_identity_accepts_client_id() {
        let provider = AzureIdentityProvider::with_managed_identity(
            "user@tenant.onmicrosoft.com",
            "03bbedd2-cce5-45ab-9414-1c1cb82361f0",
        )
        .unwrap();
        assert_eq!(provider.identity_name, "user@tenant.onmicrosoft.com");
        assert_eq!(provider.scope, DEFAULT_AZURE_SCOPE);
    }

    #[test]
    fn test_with_scope_overrides_default() {
        let provider = AzureIdentityProvider::with_managed_identity(
            "user@tenant.onmicrosoft.com",
            "client-id",
        )
        .unwrap()
        .with_scope("https://custom.scope/.default");
        assert_eq!(provider.scope, "https://custom.scope/.default");
    }

    #[test]
    fn test_provider_is_cloneable() {
        let provider = AzureIdentityProvider::with_managed_identity(
            "user@tenant.onmicrosoft.com",
            "client-id",
        )
        .unwrap();
        let cloned = provider.clone();
        assert_eq!(cloned.identity_name, provider.identity_name);
        assert_eq!(cloned.scope, provider.scope);
    }

    #[test]
    fn test_provider_as_trait_object() {
        let provider: Box<dyn IdentityProvider> = Box::new(
            AzureIdentityProvider::with_managed_identity(
                "user@tenant.onmicrosoft.com",
                "client-id",
            )
            .unwrap(),
        );
        let _cloned = provider.clone();
    }

    #[test]
    fn test_with_default_credentials() {
        // Test that the function creates a provider
        let provider =
            AzureIdentityProvider::with_default_credentials("user@tenant.onmicrosoft.com")
                .expect("Failed to create default credentials provider");

        assert_eq!(provider.identity_name, "user@tenant.onmicrosoft.com");
        assert_eq!(provider.scope, DEFAULT_AZURE_SCOPE);
    }

    #[test]
    fn test_default_credentials_with_custom_scope() {
        let provider =
            AzureIdentityProvider::with_default_credentials("user@tenant.onmicrosoft.com")
                .expect("Failed to create provider")
                .with_scope("https://custom.scope/.default");

        assert_eq!(provider.scope, "https://custom.scope/.default");
    }

    #[test]
    fn test_multiple_providers_with_different_scopes() {
        let provider1 = AzureIdentityProvider::with_managed_identity(
            "user1@tenant.onmicrosoft.com",
            "client-id-1",
        )
        .unwrap()
        .with_scope("https://scope1.com/.default");

        let provider2 = AzureIdentityProvider::with_managed_identity(
            "user2@tenant.onmicrosoft.com",
            "client-id-2",
        )
        .unwrap()
        .with_scope("https://scope2.com/.default");

        assert_eq!(provider1.identity_name, "user1@tenant.onmicrosoft.com");
        assert_eq!(provider1.scope, "https://scope1.com/.default");

        assert_eq!(provider2.identity_name, "user2@tenant.onmicrosoft.com");
        assert_eq!(provider2.scope, "https://scope2.com/.default");
    }

    #[test]
    fn test_identity_name_formats() {
        // Test different valid identity name formats
        let formats = vec![
            "user@tenant.onmicrosoft.com",
            "first.last@company.com",
            "user_name@domain.com",
            "user-name@sub.domain.com",
        ];

        for identity_name in formats {
            let provider =
                AzureIdentityProvider::with_managed_identity(identity_name, "client-id").unwrap();
            assert_eq!(provider.identity_name, identity_name);
        }
    }

    #[test]
    fn test_scope_formats() {
        let provider = AzureIdentityProvider::with_managed_identity(
            "user@tenant.onmicrosoft.com",
            "client-id",
        )
        .unwrap();

        // Test different scope formats
        let scopes = vec![
            "https://ossrdbms-aad.database.windows.net/.default",
            "https://management.azure.com/.default",
            "https://graph.microsoft.com/.default",
            "api://custom-api/.default",
        ];

        for scope in scopes {
            let p = provider.clone().with_scope(scope);
            assert_eq!(p.scope, scope);
        }
    }

    #[test]
    fn test_provider_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AzureIdentityProvider>();
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    /// Integration test for managed identity
    /// Only works when running in Azure with managed identity configured
    #[tokio::test]
    #[ignore]
    async fn test_managed_identity_authentication_real() {
        let provider = AzureIdentityProvider::new("user@tenant.onmicrosoft.com")
            .expect("Failed to create managed identity provider");

        let credentials = provider.get_credentials().await.expect(
            "Failed to get credentials. This test only works in Azure with managed identity.",
        );

        match credentials {
            Credentials::Token { username, token } => {
                assert_eq!(username, "user@tenant.onmicrosoft.com");
                assert!(!token.is_empty());
                println!("✓ Successfully authenticated with Managed Identity");
            }
            _ => panic!("Expected Token credentials"),
        }
    }

    /// Integration test for default credential chain
    /// Works with az login locally or managed identity in Azure
    #[tokio::test]
    #[ignore]
    async fn test_default_credentials_authentication_real() {
        let provider =
            AzureIdentityProvider::with_default_credentials("user@tenant.onmicrosoft.com")
                .expect("Failed to create default credentials provider");

        let credentials = provider
            .get_credentials()
            .await
            .expect("Failed to get credentials. Make sure 'az login' was run or managed identity is configured.");

        match credentials {
            Credentials::Token { username, token } => {
                assert_eq!(username, "user@tenant.onmicrosoft.com");
                assert!(!token.is_empty());
                assert!(token.len() > 100);
                println!("✓ Successfully authenticated with Default Credentials");
                println!("  Token length: {}", token.len());
            }
            _ => panic!("Expected Token credentials"),
        }
    }
}
