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

//! Plugin descriptor for the Dataverse source.

use crate::DataverseSourceConfig;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

// ── DTO types ────────────────────────────────────────────────────────────────

fn default_min_interval_ms() -> ConfigValue<u64> {
    ConfigValue::Static(500)
}

fn default_max_interval_seconds() -> ConfigValue<u64> {
    ConfigValue::Static(30)
}

fn default_api_version() -> ConfigValue<String> {
    ConfigValue::Static("v9.2".to_string())
}

/// Configuration DTO for the Dataverse source plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::dataverse::DataverseSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DataverseSourceConfigDto {
    /// Dataverse environment URL (e.g., `https://myorg.crm.dynamics.com`).
    pub environment_url: ConfigValue<String>,

    /// Azure AD tenant ID for OAuth2 authentication.
    #[serde(default)]
    pub tenant_id: Option<ConfigValue<String>>,

    /// Azure AD application (client) ID.
    #[serde(default)]
    pub client_id: Option<ConfigValue<String>>,

    /// Azure AD client secret for OAuth2 client credentials flow.
    #[serde(default)]
    pub client_secret: Option<ConfigValue<String>>,

    /// Use Azure CLI for authentication instead of client credentials.
    #[serde(default)]
    pub use_azure_cli: bool,

    /// Entity logical names to monitor (e.g., `["account", "contact"]`).
    pub entities: Vec<String>,

    /// Override entity set names for non-standard pluralization.
    #[serde(default)]
    pub entity_set_overrides: HashMap<String, String>,

    /// Per-entity column selection.
    #[serde(default)]
    pub entity_columns: HashMap<String, Vec<String>>,

    /// Minimum adaptive polling interval in milliseconds.
    #[serde(default = "default_min_interval_ms")]
    pub min_interval_ms: ConfigValue<u64>,

    /// Maximum adaptive polling interval per entity in seconds.
    /// Scaled by sqrt(entity_count) at startup.
    #[serde(default = "default_max_interval_seconds")]
    pub max_interval_seconds: ConfigValue<u64>,

    /// Dataverse Web API version (default: `v9.2`).
    #[serde(default = "default_api_version")]
    pub api_version: ConfigValue<String>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(DataverseSourceConfigDto)))]
struct DataverseSourceSchemas;

/// Plugin descriptor for the Dataverse source.
pub struct DataverseSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for DataverseSourceDescriptor {
    fn kind(&self) -> &str {
        "dataverse"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.dataverse.DataverseSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = DataverseSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: DataverseSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let environment_url = mapper.resolve_string(&dto.environment_url)?;
        let tenant_id = mapper
            .resolve_optional_string(&dto.tenant_id)?
            .unwrap_or_default();
        let client_id = mapper
            .resolve_optional_string(&dto.client_id)?
            .unwrap_or_default();
        let client_secret = mapper
            .resolve_optional_string(&dto.client_secret)?
            .unwrap_or_default();
        let min_interval_ms = mapper.resolve_typed(&dto.min_interval_ms)?;
        let max_interval_seconds = mapper.resolve_typed(&dto.max_interval_seconds)?;
        let api_version = mapper.resolve_string(&dto.api_version)?;

        let config = DataverseSourceConfig {
            environment_url,
            tenant_id,
            client_id,
            client_secret,
            use_azure_cli: dto.use_azure_cli,
            entities: dto.entities,
            entity_set_overrides: dto.entity_set_overrides,
            entity_columns: dto.entity_columns,
            min_interval_ms,
            max_interval_seconds,
            api_version,
        };

        let source = crate::DataverseSourceBuilder::new(id)
            .with_environment_url(config.environment_url.clone())
            .with_tenant_id(config.tenant_id.clone())
            .with_client_id(config.client_id.clone())
            .with_client_secret(config.client_secret.clone())
            .with_entities(config.entities.clone())
            .with_min_interval_ms(config.min_interval_ms)
            .with_max_interval_seconds(config.max_interval_seconds)
            .with_api_version(config.api_version.clone())
            .with_auto_start(auto_start);

        // Apply entity set overrides
        let mut source = source;
        for (entity, set_name) in &config.entity_set_overrides {
            source = source.with_entity_set_override(entity, set_name);
        }

        // Apply per-entity column selections
        for (entity, columns) in &config.entity_columns {
            source = source.with_entity_columns(entity, columns.clone());
        }

        let source = source.build()?;

        Ok(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dto_deserializes_from_camel_case_json() {
        let json = serde_json::json!({
            "environmentUrl": "https://myorg.crm.dynamics.com",
            "tenantId": "tenant-1",
            "clientId": "client-1",
            "clientSecret": "secret-1",
            "entities": ["account", "contact"]
        });

        let dto: DataverseSourceConfigDto =
            serde_json::from_value(json).expect("should deserialize");

        assert_eq!(
            dto.environment_url,
            ConfigValue::Static("https://myorg.crm.dynamics.com".to_string())
        );
        assert_eq!(
            dto.tenant_id,
            Some(ConfigValue::Static("tenant-1".to_string()))
        );
        assert_eq!(
            dto.client_id,
            Some(ConfigValue::Static("client-1".to_string()))
        );
        assert_eq!(
            dto.client_secret,
            Some(ConfigValue::Static("secret-1".to_string()))
        );
        assert_eq!(dto.entities, vec!["account", "contact"]);
    }

    #[test]
    fn test_dto_applies_defaults() {
        let json = serde_json::json!({
            "environmentUrl": "https://myorg.crm.dynamics.com",
            "entities": ["account"]
        });

        let dto: DataverseSourceConfigDto =
            serde_json::from_value(json).expect("should deserialize");

        assert_eq!(dto.min_interval_ms, ConfigValue::Static(500));
        assert_eq!(dto.max_interval_seconds, ConfigValue::Static(30));
        assert_eq!(dto.api_version, ConfigValue::Static("v9.2".to_string()));
        assert!(!dto.use_azure_cli);
        assert!(dto.entity_set_overrides.is_empty());
        assert!(dto.entity_columns.is_empty());
        assert_eq!(dto.tenant_id, None);
        assert_eq!(dto.client_id, None);
        assert_eq!(dto.client_secret, None);
    }

    #[test]
    fn test_dto_azure_cli_mode() {
        let json = serde_json::json!({
            "environmentUrl": "https://myorg.crm.dynamics.com",
            "useAzureCli": true,
            "entities": ["account"]
        });

        let dto: DataverseSourceConfigDto =
            serde_json::from_value(json).expect("should deserialize");

        assert!(dto.use_azure_cli);
        assert_eq!(dto.tenant_id, None);
    }

    #[test]
    fn test_descriptor_metadata() {
        let desc = DataverseSourceDescriptor;
        assert_eq!(desc.kind(), "dataverse");
        assert_eq!(desc.config_version(), "1.0.0");
        assert_eq!(
            desc.config_schema_name(),
            "source.dataverse.DataverseSourceConfig"
        );
        // Schema JSON should be valid JSON
        let schema = desc.config_schema_json();
        let _: serde_json::Value =
            serde_json::from_str(&schema).expect("schema should be valid JSON");
    }
}
