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

//! Plugin descriptor and configuration DTO for the RocksDB index backend.
//!
//! This module is gated behind the `plugin-descriptor` cargo feature. It lets
//! the RocksDB backend participate in the standard Drasi plugin configuration
//! pipeline, so the `path` (and other fields) can be sourced from environment
//! variables or secrets via [`ConfigValue`].

use std::sync::Arc;

use anyhow::Context;
use drasi_core::interface::IndexBackendPlugin;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::RocksDbIndexProvider;

fn default_false() -> ConfigValueBool {
    ConfigValue::Static(false)
}

/// Configuration DTO for the RocksDB index backend plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RocksDbIndexConfigDto {
    /// Base directory for RocksDB data files.
    #[schema(value_type = ConfigValueString)]
    pub path: ConfigValue<String>,

    /// Enable the archive index for point-in-time queries.
    #[serde(default = "default_false")]
    #[schema(value_type = ConfigValueBool)]
    pub enable_archive: ConfigValue<bool>,

    /// Use direct I/O (recommended for SSDs).
    #[serde(default = "default_false")]
    #[schema(value_type = ConfigValueBool)]
    pub direct_io: ConfigValue<bool>,
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(RocksDbIndexConfigDto)))]
struct RocksDbIndexSchemas;

/// Descriptor for the RocksDB index backend plugin.
pub struct RocksDbIndexDescriptor;

#[async_trait]
impl IndexBackendPluginDescriptor for RocksDbIndexDescriptor {
    fn kind(&self) -> &str {
        "rocksdb"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = RocksDbIndexSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "index.rocksdb.RocksDbIndexConfig"
    }

    fn display_name(&self) -> Option<&str> {
        Some("RocksDB Index Backend")
    }

    fn display_description(&self) -> Option<&str> {
        Some("Persistent index storage backed by an embedded RocksDB database.")
    }

    async fn create_index_backend(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Arc<dyn IndexBackendPlugin>> {
        let dto: RocksDbIndexConfigDto = serde_json::from_value(config_json.clone())
            .context("Failed to deserialize RocksDB index backend configuration")?;

        let mapper = DtoMapper::new();
        let path = mapper
            .resolve_string(&dto.path)
            .await
            .context("Failed to resolve RocksDB index 'path'")?;
        let enable_archive = mapper
            .resolve_typed(&dto.enable_archive)
            .await
            .context("Failed to resolve RocksDB index 'enableArchive'")?;
        let direct_io = mapper
            .resolve_typed(&dto.direct_io)
            .await
            .context("Failed to resolve RocksDB index 'directIo'")?;

        if path.trim().is_empty() {
            anyhow::bail!("RocksDB index 'path' must not be empty");
        }

        Ok(Arc::new(RocksDbIndexProvider::new(
            path,
            enable_archive,
            direct_io,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_descriptor_metadata() {
        let d = RocksDbIndexDescriptor;
        assert_eq!(d.kind(), "rocksdb");
        assert_eq!(d.config_version(), "1.0.0");
        assert_eq!(d.config_schema_name(), "index.rocksdb.RocksDbIndexConfig");
        let schema = d.config_schema_json();
        assert!(schema.contains("RocksDbIndexConfigDto"));
    }

    #[test]
    fn test_dto_roundtrip_minimal() {
        let json = serde_json::json!({ "path": "/data/drasi" });
        let dto: RocksDbIndexConfigDto = serde_json::from_value(json).expect("deserialize");
        assert!(matches!(dto.enable_archive, ConfigValue::Static(false)));
        assert!(matches!(dto.direct_io, ConfigValue::Static(false)));
    }

    #[tokio::test]
    async fn test_create_with_static_values() {
        let json = serde_json::json!({
            "path": "/tmp/drasi-rocks-test",
            "enableArchive": true,
            "directIo": false
        });
        let provider = RocksDbIndexDescriptor
            .create_index_backend(&json)
            .await
            .expect("create");
        assert!(!provider.is_volatile());
    }

    #[tokio::test]
    async fn test_create_with_env_var_path() {
        std::env::set_var("TEST_ROCKSDB_PATH", "/tmp/drasi-rocks-env");
        let json = serde_json::json!({
            "path": { "kind": "EnvironmentVariable", "name": "TEST_ROCKSDB_PATH" }
        });
        let provider = RocksDbIndexDescriptor
            .create_index_backend(&json)
            .await
            .expect("create");
        assert!(!provider.is_volatile());
        std::env::remove_var("TEST_ROCKSDB_PATH");
    }

    #[tokio::test]
    async fn test_create_rejects_empty_path() {
        let json = serde_json::json!({ "path": "" });
        let err = match RocksDbIndexDescriptor.create_index_backend(&json).await {
            Ok(_) => panic!("should reject empty path"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("path"));
    }
}
