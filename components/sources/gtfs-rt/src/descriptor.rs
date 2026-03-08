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

use crate::config::{GtfsRtSourceConfig, InitialCursorMode};
use crate::GtfsRtSourceBuilder;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

fn default_poll_interval_secs() -> ConfigValue<u64> {
    ConfigValue::Static(30)
}

fn default_timeout_secs() -> ConfigValue<u64> {
    ConfigValue::Static(15)
}

fn default_language() -> ConfigValue<String> {
    ConfigValue::Static("en".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::gtfs_rt::GtfsRtSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GtfsRtSourceConfigDto {
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default)]
    pub trip_updates_url: Option<ConfigValue<String>>,

    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default)]
    pub vehicle_positions_url: Option<ConfigValue<String>>,

    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default)]
    pub alerts_url: Option<ConfigValue<String>>,

    #[serde(default = "default_poll_interval_secs")]
    #[schema(value_type = ConfigValueU64)]
    pub poll_interval_secs: ConfigValue<u64>,

    #[serde(default)]
    pub headers: HashMap<String, String>,

    #[serde(default = "default_timeout_secs")]
    #[schema(value_type = ConfigValueU64)]
    pub timeout_secs: ConfigValue<u64>,

    #[serde(default = "default_language")]
    #[schema(value_type = ConfigValueString)]
    pub language: ConfigValue<String>,

    #[serde(default)]
    pub start_from_now: bool,

    #[serde(default)]
    pub start_from_timestamp_ms: Option<i64>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(GtfsRtSourceConfigDto,)))]
struct GtfsRtSourceSchemas;

pub struct GtfsRtSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for GtfsRtSourceDescriptor {
    fn kind(&self) -> &str {
        "gtfs-rt"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.gtfs_rt.GtfsRtSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GtfsRtSourceSchemas::openapi();
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
        let dto: GtfsRtSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let initial_cursor_mode = if let Some(ts) = dto.start_from_timestamp_ms {
            InitialCursorMode::StartFromTimestamp(ts)
        } else if dto.start_from_now {
            InitialCursorMode::StartFromNow
        } else {
            InitialCursorMode::StartFromBeginning
        };

        let config = GtfsRtSourceConfig {
            trip_updates_url: mapper.resolve_optional_string(&dto.trip_updates_url)?,
            vehicle_positions_url: mapper.resolve_optional_string(&dto.vehicle_positions_url)?,
            alerts_url: mapper.resolve_optional_string(&dto.alerts_url)?,
            poll_interval_secs: mapper.resolve_typed(&dto.poll_interval_secs)?,
            headers: dto.headers,
            timeout_secs: mapper.resolve_typed(&dto.timeout_secs)?,
            language: mapper.resolve_string(&dto.language)?,
            initial_cursor_mode,
        };

        config.validate()?;

        let source = GtfsRtSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
