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
use std::time::Duration;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AzureAuthMethod {
    #[default]
    ManagedIdentity,
    ManagedIdentityUserAssigned,
    WorkloadIdentity,
    DeveloperTools,
    ClientSecret,
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
        let vault_url = validate_vault_url(&config.vault_url)?;
        let credential = create_credential(config)?;
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| anyhow!("Failed to create HTTP client: {e}"))?;

        Ok(Self {
            credential,
            vault_url,
            http_client,
        })
    }
}

fn validate_vault_url(vault_url: &str) -> anyhow::Result<String> {
    const ALLOWED_HOST_SUFFIXES: [&str; 4] = [
        ".vault.azure.net",
        ".vault.azure.cn",
        ".vault.usgovcloudapi.net",
        ".vault.microsoftazure.de",
    ];

    let parsed = url::Url::parse(vault_url)
        .map_err(|e| anyhow!("Invalid Azure Key Vault URL '{vault_url}': {e}"))?;

    if parsed.scheme() != "https" {
        return Err(anyhow!("Azure Key Vault URL must use HTTPS: '{vault_url}'"));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("Azure Key Vault URL must include a host: '{vault_url}'"))?;

    if !ALLOWED_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
    {
        return Err(anyhow!(
            "Azure Key Vault host '{host}' is not allowed; expected one of: {}",
            ALLOWED_HOST_SUFFIXES.join(", ")
        ));
    }

    Ok(vault_url.trim_end_matches('/').to_string())
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

        let encoded_name = urlencoding::encode(name);
        let url = format!(
            "{}/secrets/{}?api-version={}",
            self.vault_url, encoded_name, KEY_VAULT_API_VERSION
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
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct TestableProvider {
        vault_url: String,
        http_client: reqwest::Client,
    }

    impl TestableProvider {
        fn new(vault_url: &str) -> Self {
            Self {
                vault_url: vault_url.trim_end_matches('/').to_string(),
                http_client: reqwest::Client::builder()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
            }
        }

        async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
            let encoded_name = urlencoding::encode(name);
            let url = format!(
                "{}/secrets/{}?api-version={}",
                self.vault_url, encoded_name, KEY_VAULT_API_VERSION
            );

            let response = self
                .http_client
                .get(&url)
                .bearer_auth("fake-token")
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

            let secret_response: KeyVaultSecretResponse =
                response.json().await.with_context(|| {
                    format!("Failed to parse Key Vault response for secret '{name}'")
                })?;

            secret_response.value.ok_or_else(|| {
                anyhow!("Secret '{name}' in Azure Key Vault did not contain a value")
            })
        }
    }

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
    fn test_vault_url_validation_rejects_non_azure_host() {
        let config = AzureKeyVaultConfigDto {
            vault_url: "https://evil.attacker.com/".to_string(),
            auth_method: AzureAuthMethod::DeveloperTools,
            client_id: None,
            tenant_id: None,
            client_secret: None,
        };
        match AzureKeyVaultSecretStoreProvider::new(&config) {
            Ok(_) => panic!("expected invalid host to be rejected"),
            Err(err) => assert!(err.to_string().contains("vault.azure.net")),
        }
    }

    #[test]
    fn test_vault_url_validation_rejects_http() {
        let config = AzureKeyVaultConfigDto {
            vault_url: "http://myvault.vault.azure.net/".to_string(),
            auth_method: AzureAuthMethod::DeveloperTools,
            client_id: None,
            tenant_id: None,
            client_secret: None,
        };
        match AzureKeyVaultSecretStoreProvider::new(&config) {
            Ok(_) => panic!("expected non-HTTPS URL to be rejected"),
            Err(err) => assert!(err.to_string().contains("HTTPS")),
        }
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
    async fn test_get_secret_success() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/secrets/my-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": "super-secret-value",
                "id": "https://myvault.vault.azure.net/secrets/my-secret/version1"
            })))
            .mount(&mock_server)
            .await;

        let provider = TestableProvider::new(&mock_server.uri());
        let result = provider.get_secret("my-secret").await.unwrap();
        assert_eq!(result, "super-secret-value");
    }

    #[tokio::test]
    async fn test_get_secret_404_not_found() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/secrets/missing-secret"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "code": "SecretNotFound",
                    "message": "A secret with (name/id) missing-secret was not found"
                }
            })))
            .mount(&mock_server)
            .await;

        let provider = TestableProvider::new(&mock_server.uri());
        let err = provider.get_secret("missing-secret").await.unwrap_err();
        assert!(err.to_string().contains("404"), "Error: {err}");
    }

    #[tokio::test]
    async fn test_get_secret_403_forbidden() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/secrets/forbidden"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
                "error": {
                    "code": "Forbidden",
                    "message": "Access denied"
                }
            })))
            .mount(&mock_server)
            .await;

        let provider = TestableProvider::new(&mock_server.uri());
        let err = provider.get_secret("forbidden").await.unwrap_err();
        assert!(err.to_string().contains("403"), "Error: {err}");
    }

    #[tokio::test]
    async fn test_get_secret_null_value() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/secrets/null-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": null,
                "id": "https://myvault.vault.azure.net/secrets/null-secret/version1"
            })))
            .mount(&mock_server)
            .await;

        let provider = TestableProvider::new(&mock_server.uri());
        let err = provider.get_secret("null-secret").await.unwrap_err();
        assert!(
            err.to_string().contains("did not contain a value"),
            "Error: {err}"
        );
    }

    #[tokio::test]
    async fn test_get_secret_malformed_json() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/secrets/bad-json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not valid json {{{"))
            .mount(&mock_server)
            .await;

        let provider = TestableProvider::new(&mock_server.uri());
        let err = provider.get_secret("bad-json").await.unwrap_err();
        assert!(
            err.to_string().contains("parse") || err.to_string().contains("Failed"),
            "Error: {err}"
        );
    }

    #[tokio::test]
    async fn test_url_encoding_prevents_path_traversal() {
        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(
                "/secrets/\\.\\.%2Fcertificates%2Fcert|/secrets/..%2Fcertificates%2Fcert",
            ))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let provider = TestableProvider::new(&mock_server.uri());
        let err = provider
            .get_secret("../certificates/cert")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"), "Error: {err}");
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
