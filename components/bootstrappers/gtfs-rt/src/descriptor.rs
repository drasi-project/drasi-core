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

use crate::{GtfsRtBootstrapConfig, GtfsRtBootstrapProvider};
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

fn default_timeout_secs() -> ConfigValue<u64> {
    ConfigValue::Static(15)
}

fn default_language() -> ConfigValue<String> {
    ConfigValue::Static("en".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::gtfs_rt::GtfsRtBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GtfsRtBootstrapConfigDto {
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default)]
    pub trip_updates_url: Option<ConfigValue<String>>,

    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default)]
    pub vehicle_positions_url: Option<ConfigValue<String>>,

    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default)]
    pub alerts_url: Option<ConfigValue<String>>,

    #[serde(default)]
    pub headers: HashMap<String, String>,

    #[serde(default = "default_timeout_secs")]
    #[schema(value_type = ConfigValueU64)]
    pub timeout_secs: ConfigValue<u64>,

    #[serde(default = "default_language")]
    #[schema(value_type = ConfigValueString)]
    pub language: ConfigValue<String>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(GtfsRtBootstrapConfigDto,)))]
struct GtfsRtBootstrapSchemas;

pub struct GtfsRtBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for GtfsRtBootstrapDescriptor {
    fn kind(&self) -> &str {
        "gtfs-rt"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.gtfs_rt.GtfsRtBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GtfsRtBootstrapSchemas::openapi();
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
        let dto: GtfsRtBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = GtfsRtBootstrapConfig {
            trip_updates_url: mapper.resolve_optional_string(&dto.trip_updates_url)?,
            vehicle_positions_url: mapper.resolve_optional_string(&dto.vehicle_positions_url)?,
            alerts_url: mapper.resolve_optional_string(&dto.alerts_url)?,
            headers: dto.headers,
            timeout_secs: mapper.resolve_typed(&dto.timeout_secs)?,
            language: mapper.resolve_string(&dto.language)?,
        };

        config.validate()?;
        Ok(Box::new(GtfsRtBootstrapProvider::new(config)?))
    }
}
