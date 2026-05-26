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

//! File-based secret store plugin for Drasi.
//!
//! Reads secrets from a JSON file on disk. Intended for development and testing.
//! The file format is a flat JSON object mapping secret names to string values:
//!
//! ```json
//! {
//!   "DB_PASSWORD": "hunter2",
//!   "API_KEY": "abc123"
//! }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use drasi_lib::secret_store::SecretStoreProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Configuration DTO for the file secret store.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileSecretStoreConfigDto {
    /// Path to the JSON secrets file.
    pub path: String,
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(FileSecretStoreConfigDto)))]
struct FileSecretStoreSchemas;

/// A secret store provider that reads secrets from a JSON file.
///
/// Secrets are loaded once at creation and cached in memory.
/// Changes to the file after creation are not reflected.
pub struct FileSecretStoreProvider {
    secrets: HashMap<String, String>,
}

impl FileSecretStoreProvider {
    /// Create a new file secret store by reading secrets from the given path.
    pub async fn new(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            anyhow::anyhow!("Failed to read secrets file '{}': {e}", path.display())
        })?;

        let secrets: HashMap<String, String> = serde_json::from_str(&content).map_err(|e| {
            anyhow::anyhow!("Failed to parse secrets file '{}': {e}", path.display())
        })?;

        Ok(Self { secrets })
    }

    /// Create a file secret store from an in-memory map (useful for testing).
    pub fn from_map(secrets: HashMap<String, String>) -> Self {
        Self { secrets }
    }
}

#[async_trait]
impl SecretStoreProvider for FileSecretStoreProvider {
    async fn get_secret(&self, name: &str) -> anyhow::Result<String> {
        self.secrets
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Secret '{name}' not found in file secret store"))
    }
}

/// Descriptor for the file secret store plugin.
pub struct FileSecretStoreDescriptor;

#[async_trait]
impl SecretStorePluginDescriptor for FileSecretStoreDescriptor {
    fn kind(&self) -> &str {
        "file"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = FileSecretStoreSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "secret_store.file.FileSecretStoreConfig"
    }

    async fn create_secret_store(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn SecretStoreProvider>> {
        let dto: FileSecretStoreConfigDto = serde_json::from_value(config_json.clone())?;
        let provider = FileSecretStoreProvider::new(&dto.path).await?;
        Ok(Box::new(provider))
    }
}

// Dynamic plugin entry point
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "secret-store-file",
    core_version = "0.4.0",
    lib_version = "0.4.0",
    plugin_version = "0.1.0",
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
    identity_provider_descriptors = [],
    secret_store_descriptors = [FileSecretStoreDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_file_secret_store_from_map() {
        let mut secrets = HashMap::new();
        secrets.insert("DB_PASSWORD".to_string(), "hunter2".to_string());
        secrets.insert("API_KEY".to_string(), "abc123".to_string());

        let store = FileSecretStoreProvider::from_map(secrets);

        assert_eq!(store.get_secret("DB_PASSWORD").await.unwrap(), "hunter2");
        assert_eq!(store.get_secret("API_KEY").await.unwrap(), "abc123");
    }

    #[tokio::test]
    async fn test_file_secret_store_missing_key() {
        let store = FileSecretStoreProvider::from_map(HashMap::new());
        let result = store.get_secret("NONEXISTENT").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn test_file_secret_store_from_file() {
        let dir = std::env::temp_dir().join("drasi_test_secrets");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("test_secrets.json");

        let secrets = serde_json::json!({
            "DB_PASSWORD": "hunter2",
            "API_KEY": "abc123"
        });
        tokio::fs::write(&path, serde_json::to_string(&secrets).unwrap())
            .await
            .unwrap();

        let store = FileSecretStoreProvider::new(&path).await.unwrap();
        assert_eq!(store.get_secret("DB_PASSWORD").await.unwrap(), "hunter2");
        assert_eq!(store.get_secret("API_KEY").await.unwrap(), "abc123");

        // Cleanup
        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_dir(&dir).await;
    }

    #[tokio::test]
    async fn test_file_secret_store_descriptor() {
        let descriptor = FileSecretStoreDescriptor;
        assert_eq!(descriptor.kind(), "file");
        assert_eq!(descriptor.config_version(), "1.0.0");
        assert!(!descriptor.config_schema_json().is_empty());
    }

    #[tokio::test]
    async fn test_file_secret_store_descriptor_create() {
        let dir = std::env::temp_dir().join("drasi_test_secrets_desc");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("desc_secrets.json");

        let secrets = serde_json::json!({"MY_SECRET": "value123"});
        tokio::fs::write(&path, serde_json::to_string(&secrets).unwrap())
            .await
            .unwrap();

        let descriptor = FileSecretStoreDescriptor;
        let config = serde_json::json!({"path": path.to_str().unwrap()});
        let store = descriptor.create_secret_store(&config).await.unwrap();
        assert_eq!(store.get_secret("MY_SECRET").await.unwrap(), "value123");

        // Cleanup
        let _ = tokio::fs::remove_file(&path).await;
        let _ = tokio::fs::remove_dir(&dir).await;
    }
}
