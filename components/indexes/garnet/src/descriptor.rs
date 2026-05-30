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

//! Plugin descriptor and configuration DTO for the Garnet/Redis index backend.
//!
//! This module is gated behind the `plugin-descriptor` cargo feature. It lets
//! the Garnet backend participate in the standard Drasi plugin configuration
//! pipeline, so the `connectionString` (and other fields) can be sourced from
//! environment variables or secrets via [`ConfigValue`].

use std::sync::Arc;

use anyhow::Context;
use drasi_core::interface::IndexBackendPlugin;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::GarnetIndexProvider;

fn default_false() -> ConfigValueBool {
    ConfigValue::Static(false)
}

/// Configuration DTO for the Garnet/Redis index backend plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct GarnetIndexConfigDto {
    /// Redis/Garnet connection string, e.g. "redis://localhost:6379".
    #[schema(value_type = ConfigValueString)]
    pub connection_string: ConfigValue<String>,

    /// Optional in-process element cache size (number of elements).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub cache_size: Option<ConfigValue<usize>>,

    /// Enable the archive index for point-in-time queries.
    #[serde(default = "default_false")]
    #[schema(value_type = ConfigValueBool)]
    pub enable_archive: ConfigValue<bool>,
}

#[derive(utoipa::OpenApi)]
#[openapi(components(schemas(GarnetIndexConfigDto)))]
struct GarnetIndexSchemas;

/// Descriptor for the Garnet/Redis index backend plugin.
pub struct GarnetIndexDescriptor;

#[async_trait]
impl IndexBackendPluginDescriptor for GarnetIndexDescriptor {
    fn kind(&self) -> &str {
        "redis"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let api = GarnetIndexSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    fn config_schema_name(&self) -> &str {
        "index.redis.GarnetIndexConfig"
    }

    fn display_name(&self) -> Option<&str> {
        Some("Redis/Garnet Index Backend")
    }

    fn display_description(&self) -> Option<&str> {
        Some("Index storage backed by a Redis-compatible (Garnet) server.")
    }

    async fn create_index_backend(
        &self,
        config_json: &serde_json::Value,
    ) -> anyhow::Result<Arc<dyn IndexBackendPlugin>> {
        let dto: GarnetIndexConfigDto = serde_json::from_value(config_json.clone())
            .context("Failed to deserialize Garnet index backend configuration")?;

        let mapper = DtoMapper::new();
        let connection_string = mapper
            .resolve_string(&dto.connection_string)
            .await
            .context("Failed to resolve Garnet index 'connectionString'")?;
        let cache_size = mapper
            .resolve_optional(&dto.cache_size)
            .await
            .context("Failed to resolve Garnet index 'cacheSize'")?;
        let enable_archive = mapper
            .resolve_typed(&dto.enable_archive)
            .await
            .context("Failed to resolve Garnet index 'enableArchive'")?;

        if connection_string.trim().is_empty() {
            anyhow::bail!("Garnet index 'connectionString' must not be empty");
        }

        Ok(Arc::new(GarnetIndexProvider::new(
            connection_string,
            cache_size,
            enable_archive,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_descriptor_metadata() {
        let d = GarnetIndexDescriptor;
        assert_eq!(d.kind(), "redis");
        assert_eq!(d.config_version(), "1.0.0");
        assert_eq!(d.config_schema_name(), "index.redis.GarnetIndexConfig");
        let schema = d.config_schema_json();
        assert!(schema.contains("GarnetIndexConfigDto"));
    }

    #[test]
    fn test_dto_roundtrip_minimal() {
        let json = serde_json::json!({ "connectionString": "redis://localhost:6379" });
        let dto: GarnetIndexConfigDto = serde_json::from_value(json).expect("deserialize");
        assert!(dto.cache_size.is_none());
        assert!(matches!(dto.enable_archive, ConfigValue::Static(false)));
    }

    #[tokio::test]
    async fn test_create_with_static_values() {
        let json = serde_json::json!({
            "connectionString": "redis://localhost:6379",
            "cacheSize": 1000,
            "enableArchive": true
        });
        let provider = GarnetIndexDescriptor
            .create_index_backend(&json)
            .await
            .expect("create");
        assert!(!provider.is_volatile());
    }

    #[tokio::test]
    async fn test_create_with_env_var_connection() {
        std::env::set_var("TEST_GARNET_CONN", "redis://example:6379");
        let json = serde_json::json!({
            "connectionString": { "kind": "EnvironmentVariable", "name": "TEST_GARNET_CONN" }
        });
        let provider = GarnetIndexDescriptor
            .create_index_backend(&json)
            .await
            .expect("create");
        assert!(!provider.is_volatile());
        std::env::remove_var("TEST_GARNET_CONN");
    }

    #[tokio::test]
    async fn test_create_rejects_empty_connection() {
        let json = serde_json::json!({ "connectionString": "" });
        let err = match GarnetIndexDescriptor.create_index_backend(&json).await {
            Ok(_) => panic!("should reject empty connection string"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("connectionString"));
    }
}
