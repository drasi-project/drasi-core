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

//! Plugin descriptor for the Dataverse bootstrap provider.

use crate::DataverseBootstrapConfig;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

// ── DTO types ────────────────────────────────────────────────────────────────

fn default_api_version() -> ConfigValue<String> {
    ConfigValue::Static("v9.2".to_string())
}

fn default_page_size() -> ConfigValue<u64> {
    ConfigValue::Static(5000)
}

/// Configuration DTO for the Dataverse bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::dataverse::DataverseBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DataverseBootstrapConfigDto {
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

    /// Entity logical names to bootstrap (e.g., `["account", "contact"]`).
    pub entities: Vec<String>,

    /// Override entity set names for non-standard pluralization.
    #[serde(default)]
    pub entity_set_overrides: HashMap<String, String>,

    /// Per-entity column selection.
    #[serde(default)]
    pub entity_columns: HashMap<String, Vec<String>>,

    /// Dataverse Web API version (default: `v9.2`).
    #[serde(default = "default_api_version")]
    pub api_version: ConfigValue<String>,

    /// Number of records per page (default: 5000).
    #[serde(default = "default_page_size")]
    pub page_size: ConfigValue<u64>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(DataverseBootstrapConfigDto)))]
struct DataverseBootstrapSchemas;

/// Plugin descriptor for the Dataverse bootstrap provider.
pub struct DataverseBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for DataverseBootstrapDescriptor {
    fn kind(&self) -> &str {
        "dataverse"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.dataverse.DataverseBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = DataverseBootstrapSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: DataverseBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
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
        let api_version = mapper.resolve_string(&dto.api_version)?;
        let page_size = mapper.resolve_typed(&dto.page_size)? as usize;

        let config = DataverseBootstrapConfig {
            environment_url,
            tenant_id,
            client_id,
            client_secret,
            use_azure_cli: dto.use_azure_cli,
            entities: dto.entities,
            entity_set_overrides: dto.entity_set_overrides,
            entity_columns: dto.entity_columns,
            api_version,
            page_size,
        };

        config.validate().map_err(|e| anyhow::anyhow!(e))?;

        let provider = crate::DataverseBootstrapProvider::new(config)?;
        Ok(Box::new(provider))
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

        let dto: DataverseBootstrapConfigDto =
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

        let dto: DataverseBootstrapConfigDto =
            serde_json::from_value(json).expect("should deserialize");

        assert_eq!(
            dto.api_version,
            ConfigValue::Static("v9.2".to_string())
        );
        assert_eq!(dto.page_size, ConfigValue::Static(5000));
        assert!(!dto.use_azure_cli);
        assert!(dto.entity_set_overrides.is_empty());
        assert!(dto.entity_columns.is_empty());
        assert_eq!(dto.tenant_id, None);
        assert_eq!(dto.client_id, None);
        assert_eq!(dto.client_secret, None);
    }

    #[test]
    fn test_descriptor_metadata() {
        let desc = DataverseBootstrapDescriptor;
        assert_eq!(desc.kind(), "dataverse");
        assert_eq!(desc.config_version(), "1.0.0");
        assert_eq!(
            desc.config_schema_name(),
            "bootstrap.dataverse.DataverseBootstrapConfig"
        );
        let schema = desc.config_schema_json();
        let _: serde_json::Value =
            serde_json::from_str(&schema).expect("schema should be valid JSON");
    }
}
