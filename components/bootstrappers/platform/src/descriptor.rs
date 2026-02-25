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

//! Plugin descriptor for the Platform bootstrap provider.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::PlatformBootstrapConfig;
use crate::PlatformBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

fn default_timeout_seconds() -> ConfigValue<u64> {
    ConfigValue::Static(300)
}

/// Configuration DTO for the Platform bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = PlatformBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformBootstrapConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub query_api_url: Option<ConfigValue<String>>,

    #[serde(default = "default_timeout_seconds")]
    #[schema(value_type = ConfigValueU64)]
    pub timeout_seconds: ConfigValue<u64>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(PlatformBootstrapConfigDto)))]
struct PlatformBootstrapSchemas;

/// Plugin descriptor for the Platform bootstrap provider.
pub struct PlatformBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for PlatformBootstrapDescriptor {
    fn kind(&self) -> &str {
        "platform"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "PlatformBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = PlatformBootstrapSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().expect("OpenAPI components missing").schemas).expect("Failed to serialize config schema")
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: PlatformBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let query_api_url = mapper.resolve_optional_string(&dto.query_api_url)?;
        let timeout_seconds = mapper.resolve_typed(&dto.timeout_seconds)?;

        let config = PlatformBootstrapConfig {
            query_api_url,
            timeout_seconds,
        };

        let provider = PlatformBootstrapProvider::new(config)?;
        Ok(Box::new(provider))
    }
}
