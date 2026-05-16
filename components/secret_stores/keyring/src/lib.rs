#![allow(unexpected_cfgs)]
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

//! Keyring-based secret store plugin for Drasi.
//!
//! Resolves secrets from the platform credential store via the `keyring` crate:
//! - macOS: Keychain
//! - Linux: Secret Service (GNOME Keyring / KWallet via D-Bus)
//! - Windows: Credential Manager

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use drasi_lib::secret_store::SecretStoreProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Configuration DTO for the keyring secret store.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct KeyringSecretStoreConfigDto {
    /// Keyring service/application name (default: "drasi")
    #[serde(default = "default_service")]
    pub service: String,
}

fn default_service() -> String {
    "drasi".to_string()
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(KeyringSecretStoreConfigDto)))]
struct KeyringSecretStoreSchemas;

/// A secret store provider that resolves secrets from the OS keyring.
pub struct KeyringSecretStoreProvider {
    service: String,
}

impl KeyringSecretStoreProvider {
    /// Create a new keyring secret store for the given service/application name.
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

#[async_trait]
impl SecretStoreProvider for KeyringSecretStoreProvider {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        let service = self.service.clone();
        let name = name.to_string();

        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(&service, &name).map_err(|e| {
                anyhow!(
                    "Failed to create keyring entry for secret '{name}' in service '{service}': {e}"
                )
            })?;

            entry.get_password().map_err(|e| {
                anyhow!("Failed to read secret '{name}' from keyring service '{service}': {e}")
            })
        })
        .await
        .context("Keyring access task failed")?
    }
}

/// Descriptor for the keyring secret store plugin.
pub struct KeyringSecretStoreDescriptor;

#[async_trait]
impl SecretStorePluginDescriptor for KeyringSecretStoreDescriptor {
    fn kind(&self) -> &str {
        "keyring"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = KeyringSecretStoreSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "secret_store.keyring.KeyringSecretStoreConfig"
    }

    async fn create_secret_store(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn SecretStoreProvider>> {
        let dto: KeyringSecretStoreConfigDto = serde_json::from_value(config_json.clone())?;
        Ok(Box::new(KeyringSecretStoreProvider::new(dto.service)))
    }
}

// Dynamic plugin entry point
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "secret-store-keyring",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
    identity_provider_descriptors = [],
    secret_store_descriptors = [KeyringSecretStoreDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_keyring_secret_store_config_default_service() {
        let dto: KeyringSecretStoreConfigDto = serde_json::from_value(serde_json::json!({}))
            .expect("config should deserialize with default service");

        assert_eq!(dto.service, "drasi");
    }

    #[tokio::test]
    async fn test_keyring_secret_store_descriptor_metadata() {
        let descriptor = KeyringSecretStoreDescriptor;
        let schema = descriptor.config_schema_json();

        assert_eq!(descriptor.kind(), "keyring");
        assert_eq!(descriptor.config_version(), "1.0.0");
        assert_eq!(
            descriptor.config_schema_name(),
            "secret_store.keyring.KeyringSecretStoreConfig"
        );
        assert!(schema.contains("\"service\""));
        assert!(schema.contains("drasi"));
    }

    #[tokio::test]
    async fn test_keyring_secret_store_descriptor_create() {
        let descriptor = KeyringSecretStoreDescriptor;
        let config = serde_json::json!({"service": "drasi-tests"});

        descriptor
            .create_secret_store(&config)
            .await
            .expect("descriptor should create provider");
    }

    #[tokio::test]
    #[ignore = "requires OS keyring access"]
    async fn test_keyring_secret_store_missing_key_fails_gracefully() {
        let store = KeyringSecretStoreProvider::new("drasi-test-missing");
        let result = store.get_secret("NONEXISTENT_SECRET").await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("NONEXISTENT_SECRET"));
    }
}
