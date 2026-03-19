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

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use azure_core::credentials::TokenCredential;
use azure_identity::{
    DeveloperToolsCredential, ManagedIdentityCredential, ManagedIdentityCredentialOptions,
    UserAssignedId, WorkloadIdentityCredential,
};
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use std::sync::Arc;

const DEFAULT_AZURE_SCOPE: &str = "https://ossrdbms-aad.database.windows.net/.default";

/// Identity provider for Azure AD authentication.
///
/// Each instance represents a single authentication method.
/// Create multiple providers if you need fallback behavior.
///
/// # Identity Name
///
/// The `identity_name` is the identity used for authentication.
/// This could be in the format `user@servername` (e.g., `myuser@myserver`)
/// or an Azure AD principal name (e.g., `user@tenant.onmicrosoft.com`),
/// depending on the target resource.
#[derive(Clone)]
pub struct AzureIdentityProvider {
    credential: Arc<dyn TokenCredential>,
    identity_name: String,
    scope: String,
}

impl AzureIdentityProvider {
    /// Create provider using system-assigned managed identity.
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
    pub fn with_managed_identity(
        identity_name: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Result<Self> {
        let options = ManagedIdentityCredentialOptions {
            user_assigned_id: Some(UserAssignedId::ClientId(client_id.into())),
            ..Default::default()
        };
        let credential = ManagedIdentityCredential::new(Some(options))
            .map_err(|e| anyhow!("Failed to create managed identity credential: {e}"))?;

        Ok(Self {
            credential: credential as Arc<dyn TokenCredential>,
            identity_name: identity_name.into(),
            scope: DEFAULT_AZURE_SCOPE.to_string(),
        })
    }

    /// Create provider using Azure default credential chain (developer tools).
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
    pub fn with_scope(mut self, scope: impl Into<String>) -> Self {
        self.scope = scope.into();
        self
    }
}

#[async_trait]
impl IdentityProvider for AzureIdentityProvider {
    async fn get_credentials(&self, _context: &CredentialContext) -> Result<Credentials> {
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

    // ---- Construction tests ----

    #[test]
    fn test_new_creates_system_assigned_managed_identity() {
        let result = AzureIdentityProvider::new("user@myserver");
        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.identity_name, "user@myserver");
        assert_eq!(provider.scope, DEFAULT_AZURE_SCOPE);
    }

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
    fn test_with_default_credentials_creates_provider() {
        let result = AzureIdentityProvider::with_default_credentials("devuser@tenant.onmicrosoft.com");
        assert!(result.is_ok());
        let provider = result.unwrap();
        assert_eq!(provider.identity_name, "devuser@tenant.onmicrosoft.com");
        assert_eq!(provider.scope, DEFAULT_AZURE_SCOPE);
    }

    #[test]
    fn test_with_workload_identity_creates_provider() {
        // WorkloadIdentityCredential may fail without AKS env vars, but should not panic.
        let _result = AzureIdentityProvider::with_workload_identity("workload@tenant.onmicrosoft.com");
    }

    // ---- Scope tests ----

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
    fn test_multiple_providers_with_different_scopes() {
        let provider_a = AzureIdentityProvider::new("user_a@server")
            .unwrap()
            .with_scope("https://scope-a/.default");
        let provider_b = AzureIdentityProvider::with_managed_identity("user_b@server", "client-id")
            .unwrap()
            .with_scope("https://scope-b/.default");

        assert_eq!(provider_a.scope, "https://scope-a/.default");
        assert_eq!(provider_b.scope, "https://scope-b/.default");
        assert_ne!(provider_a.identity_name, provider_b.identity_name);
    }

    // ---- Clone & trait object tests ----

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
    fn test_clone_box_returns_valid_trait_object() {
        let provider = AzureIdentityProvider::new("user@myserver").unwrap();
        let boxed: Box<dyn IdentityProvider> = provider.clone_box();
        // Cloned trait object should also be cloneable
        let _boxed2: Box<dyn IdentityProvider> = boxed.clone_box();
    }

    #[test]
    fn test_provider_as_trait_object() {
        let provider = AzureIdentityProvider::new("user@myserver").unwrap();
        let _trait_obj: Box<dyn IdentityProvider> = Box::new(provider);
    }

    #[test]
    fn test_provider_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AzureIdentityProvider>();
    }

    // ---- Identity name format tests ----

    #[test]
    fn test_identity_name_user_at_server_format() {
        let provider = AzureIdentityProvider::new("myuser@myserver").unwrap();
        assert_eq!(provider.identity_name, "myuser@myserver");
    }

    #[test]
    fn test_identity_name_aad_principal_format() {
        let provider = AzureIdentityProvider::new("admin@contoso.onmicrosoft.com").unwrap();
        assert_eq!(provider.identity_name, "admin@contoso.onmicrosoft.com");
    }
}
