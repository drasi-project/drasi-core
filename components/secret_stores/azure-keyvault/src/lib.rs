#![allow(unexpected_cfgs)]
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

//! Azure Key Vault secret store plugin for Drasi.
//!
//! Resolves named secrets from Azure Key Vault using Azure Identity credentials
//! and the Key Vault REST API directly (to avoid SDK version conflicts with
//! the azure_core 0.31 track used by the rest of the workspace).

use std::sync::Arc;

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use azure_core::credentials::{Secret, TokenCredential};
use azure_identity::{
    ClientSecretCredential, DeveloperToolsCredential, ManagedIdentityCredential,
    ManagedIdentityCredentialOptions, UserAssignedId, WorkloadIdentityCredential,
};
use drasi_lib::secret_store::SecretStoreProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

const KEY_VAULT_SCOPE: &str = "https://vault.azure.net/.default";
const KEY_VAULT_API_VERSION: &str = "7.4";

/// Configuration DTO for the Azure Key Vault secret store plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AzureKeyVaultConfigDto {
    /// Key Vault URL, e.g. "https://myvault.vault.azure.net/"
    pub vault_url: String,

    /// Authentication method
    #[serde(default)]
    pub auth_method: AzureAuthMethod,

    /// Client ID for user-assigned managed identity or client secret auth
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,

    /// Tenant ID for client secret auth
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,

    /// Client secret for service principal auth
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AzureAuthMethod {
    ManagedIdentity,
    ManagedIdentityUserAssigned,
    WorkloadIdentity,
    DeveloperTools,
    ClientSecret,
}

impl Default for AzureAuthMethod {
    fn default() -> Self {
        Self::ManagedIdentity
    }
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(AzureKeyVaultConfigDto, AzureAuthMethod)))]
struct AzureKeyVaultSecretStoreSchemas;

/// Response from the Key Vault REST API `GET /secrets/{name}` endpoint.
#[derive(Deserialize)]
struct KeyVaultSecretResponse {
    value: Option<String>,
}

pub struct AzureKeyVaultSecretStoreProvider {
    credential: Arc<dyn TokenCredential>,
    vault_url: String,
    http_client: reqwest::Client,
}

impl AzureKeyVaultSecretStoreProvider {
    pub fn new(config: &AzureKeyVaultConfigDto) -> anyhow::Result<Self> {
        let credential = create_credential(config)?;
        let vault_url = config.vault_url.trim_end_matches('/').to_string();
        let http_client = reqwest::Client::new();

        Ok(Self {
            credential,
            vault_url,
            http_client,
        })
    }
}

fn create_credential(config: &AzureKeyVaultConfigDto) -> anyhow::Result<Arc<dyn TokenCredential>> {
    match config.auth_method {
        AzureAuthMethod::ManagedIdentity => ManagedIdentityCredential::new(None)
            .map(|credential| credential as Arc<dyn TokenCredential>)
            .map_err(|e| anyhow!("Failed to create managed identity credential: {e}")),
        AzureAuthMethod::ManagedIdentityUserAssigned => {
            let client_id = config.client_id.clone().ok_or_else(|| {
                anyhow!("client_id is required for managed_identity_user_assigned auth method")
            })?;
            let options = ManagedIdentityCredentialOptions {
                user_assigned_id: Some(UserAssignedId::ClientId(client_id)),
                ..Default::default()
            };

            ManagedIdentityCredential::new(Some(options))
                .map(|credential| credential as Arc<dyn TokenCredential>)
                .map_err(|e| anyhow!("Failed to create managed identity credential: {e}"))
        }
        AzureAuthMethod::WorkloadIdentity => WorkloadIdentityCredential::new(None)
            .map(|credential| credential as Arc<dyn TokenCredential>)
            .map_err(|e| anyhow!("Failed to create workload identity credential: {e}")),
        AzureAuthMethod::DeveloperTools => DeveloperToolsCredential::new(None)
            .map(|credential| credential as Arc<dyn TokenCredential>)
            .map_err(|e| anyhow!("Failed to create developer tools credential: {e}")),
        AzureAuthMethod::ClientSecret => {
            let tenant_id = config
                .tenant_id
                .as_deref()
                .ok_or_else(|| anyhow!("tenant_id is required for client_secret auth method"))?;
            let client_id = config
                .client_id
                .clone()
                .ok_or_else(|| anyhow!("client_id is required for client_secret auth method"))?;
            let client_secret = config.client_secret.clone().ok_or_else(|| {
                anyhow!("client_secret is required for client_secret auth method")
            })?;

            ClientSecretCredential::new(tenant_id, client_id, Secret::new(client_secret), None)
                .map(|credential| credential as Arc<dyn TokenCredential>)
                .map_err(|e| anyhow!("Failed to create client secret credential: {e}"))
        }
    }
}

#[async_trait]
impl SecretStoreProvider for AzureKeyVaultSecretStoreProvider {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        // Acquire a bearer token for Key Vault
        let token_response = self
            .credential
            .get_token(&[KEY_VAULT_SCOPE], None)
            .await
            .with_context(|| {
                format!("Failed to acquire Azure token for Key Vault secret '{name}'")
            })?;

        let url = format!(
            "{}/secrets/{}?api-version={}",
            self.vault_url, name, KEY_VAULT_API_VERSION
        );

        let response = self
            .http_client
            .get(&url)
            .bearer_auth(token_response.token.secret())
            .send()
            .await
            .with_context(|| format!("HTTP request failed for Key Vault secret '{name}'"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Key Vault returned {status} for secret '{name}': {body}"
            ));
        }

        let secret_response: KeyVaultSecretResponse = response
            .json()
            .await
            .with_context(|| format!("Failed to parse Key Vault response for secret '{name}'"))?;

        secret_response
            .value
            .ok_or_else(|| anyhow!("Secret '{name}' in Azure Key Vault did not contain a value"))
    }
}

/// Descriptor for the Azure Key Vault secret store plugin.
pub struct AzureKeyVaultSecretStoreDescriptor;

#[async_trait]
impl SecretStorePluginDescriptor for AzureKeyVaultSecretStoreDescriptor {
    fn kind(&self) -> &str {
        "azure-keyvault"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = AzureKeyVaultSecretStoreSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "secret_store.azure_keyvault.AzureKeyVaultConfig"
    }

    async fn create_secret_store(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn SecretStoreProvider>> {
        let dto: AzureKeyVaultConfigDto = serde_json::from_value(config_json.clone())?;
        let provider = AzureKeyVaultSecretStoreProvider::new(&dto)?;
        Ok(Box::new(provider))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "secret-store-azure-keyvault",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
    identity_provider_descriptors = [],
    secret_store_descriptors = [AzureKeyVaultSecretStoreDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_dto_deserialization_defaults_auth_method() {
        let json = serde_json::json!({
            "vaultUrl": "https://example.vault.azure.net/"
        });

        let config: AzureKeyVaultConfigDto = serde_json::from_value(json).unwrap();
        assert_eq!(config.vault_url, "https://example.vault.azure.net/");
        assert_eq!(config.auth_method, AzureAuthMethod::ManagedIdentity);
        assert_eq!(config.client_id, None);
        assert_eq!(config.tenant_id, None);
        assert_eq!(config.client_secret, None);
    }

    #[test]
    fn test_config_dto_deserialization_client_secret() {
        let json = serde_json::json!({
            "vaultUrl": "https://example.vault.azure.net/",
            "authMethod": "client_secret",
            "clientId": "client-id",
            "tenantId": "tenant-id",
            "clientSecret": "client-secret"
        });

        let config: AzureKeyVaultConfigDto = serde_json::from_value(json).unwrap();
        assert_eq!(config.auth_method, AzureAuthMethod::ClientSecret);
        assert_eq!(config.client_id.as_deref(), Some("client-id"));
        assert_eq!(config.tenant_id.as_deref(), Some("tenant-id"));
        assert_eq!(config.client_secret.as_deref(), Some("client-secret"));
    }

    #[test]
    fn test_config_dto_deserialization_all_methods() {
        for method in &[
            "managed_identity",
            "managed_identity_user_assigned",
            "workload_identity",
            "developer_tools",
            "client_secret",
        ] {
            let json = serde_json::json!({
                "vaultUrl": "https://example.vault.azure.net/",
                "authMethod": method,
            });
            let config: AzureKeyVaultConfigDto = serde_json::from_value(json).unwrap();
            assert_eq!(config.vault_url, "https://example.vault.azure.net/");
        }
    }

    #[test]
    fn test_vault_url_trailing_slash_trimmed() {
        let config = AzureKeyVaultConfigDto {
            vault_url: "https://myvault.vault.azure.net/".to_string(),
            auth_method: AzureAuthMethod::DeveloperTools,
            client_id: None,
            tenant_id: None,
            client_secret: None,
        };
        let provider = AzureKeyVaultSecretStoreProvider::new(&config).unwrap();
        assert_eq!(provider.vault_url, "https://myvault.vault.azure.net");
    }

    #[test]
    fn test_descriptor_metadata() {
        let descriptor = AzureKeyVaultSecretStoreDescriptor;
        assert_eq!(descriptor.kind(), "azure-keyvault");
        assert_eq!(descriptor.config_version(), "1.0.0");
        assert_eq!(
            descriptor.config_schema_name(),
            "secret_store.azure_keyvault.AzureKeyVaultConfig"
        );
        let schema = descriptor.config_schema_json();
        assert!(schema.contains("AzureKeyVaultConfigDto"));
        assert!(schema.contains("AzureAuthMethod"));
    }

    #[test]
    fn test_create_secret_store_requires_client_secret_fields() {
        let config = serde_json::json!({
            "vaultUrl": "https://example.vault.azure.net/",
            "authMethod": "client_secret",
            "clientId": "client-id"
        });

        let descriptor = AzureKeyVaultSecretStoreDescriptor;
        let result = drasi_plugin_sdk::__tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(descriptor.create_secret_store(&config));
        match result {
            Ok(_) => panic!("should fail without tenant_id"),
            Err(e) => assert!(e.to_string().contains("tenant_id is required")),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn test_live_get_secret_with_developer_tools() {
        let vault_url = std::env::var("DRASI_TEST_AZURE_KEY_VAULT_URL")
            .expect("DRASI_TEST_AZURE_KEY_VAULT_URL must be set");
        let secret_name = std::env::var("DRASI_TEST_AZURE_KEY_VAULT_SECRET_NAME")
            .expect("DRASI_TEST_AZURE_KEY_VAULT_SECRET_NAME must be set");

        let provider = AzureKeyVaultSecretStoreProvider::new(&AzureKeyVaultConfigDto {
            vault_url,
            auth_method: AzureAuthMethod::DeveloperTools,
            client_id: None,
            tenant_id: None,
            client_secret: None,
        })
        .expect("provider should construct");

        let secret = provider
            .get_secret(&secret_name)
            .await
            .expect("secret value");
        assert!(!secret.is_empty());
    }
}
