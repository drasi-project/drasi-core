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

//! Open511 source plugin descriptor and DTOs.

use crate::config::{InitialCursorBehavior, Open511SourceConfig};
use crate::Open511SourceBuilder;
use drasi_plugin_sdk::prelude::*;
use std::str::FromStr;
use utoipa::OpenApi;

/// DTO for initial cursor behavior.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default, utoipa::ToSchema)]
#[schema(as = source::open511::InitialCursorBehavior)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InitialCursorBehaviorDto {
    #[default]
    StartFromBeginning,
    StartFromNow,
    StartFromTimestamp {
        timestamp_millis: i64,
    },
}

impl FromStr for InitialCursorBehaviorDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.trim();
        let lower = value.to_ascii_lowercase();

        if matches!(
            lower.as_str(),
            "start_from_beginning" | "startfrombeginning" | "beginning"
        ) {
            return Ok(Self::StartFromBeginning);
        }

        if matches!(lower.as_str(), "start_from_now" | "startfromnow" | "now") {
            return Ok(Self::StartFromNow);
        }

        for prefix in [
            "start_from_timestamp:",
            "startfromtimestamp:",
            "timestamp:",
            "start_from_timestamp=",
            "startfromtimestamp=",
            "timestamp=",
        ] {
            if let Some(raw_millis) = lower.strip_prefix(prefix) {
                let timestamp_millis = raw_millis
                    .parse::<i64>()
                    .map_err(|e| format!("Invalid timestamp millis '{raw_millis}': {e}"))?;
                return Ok(Self::StartFromTimestamp { timestamp_millis });
            }
        }

        Err(format!(
            "Invalid initial cursor behavior '{value}'. Expected one of: \
start_from_beginning, start_from_now, start_from_timestamp:<millis>"
        ))
    }
}

/// Open511 source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::open511::Open511SourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Open511SourceConfigDto {
    pub base_url: ConfigValue<String>,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: ConfigValue<u64>,
    #[serde(default = "default_full_sweep_interval")]
    pub full_sweep_interval: ConfigValue<u32>,
    #[serde(default = "default_request_timeout_secs")]
    pub request_timeout_secs: ConfigValue<u64>,
    #[serde(default = "default_page_size")]
    pub page_size: ConfigValue<usize>,
    #[serde(
        default = "default_status_filter",
        skip_serializing_if = "Option::is_none"
    )]
    pub status_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_type_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area_id_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub road_name_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jurisdiction_filter: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox_filter: Option<ConfigValue<String>>,
    #[serde(default)]
    pub auto_delete_archived: ConfigValue<bool>,
    #[serde(default = "default_initial_cursor_behavior")]
    #[schema(value_type = ConfigValue<source::open511::InitialCursorBehavior>)]
    pub initial_cursor_behavior: ConfigValue<InitialCursorBehaviorDto>,
}

fn default_poll_interval_secs() -> ConfigValue<u64> {
    ConfigValue::Static(60)
}

fn default_full_sweep_interval() -> ConfigValue<u32> {
    ConfigValue::Static(10)
}

fn default_request_timeout_secs() -> ConfigValue<u64> {
    ConfigValue::Static(15)
}

fn default_page_size() -> ConfigValue<usize> {
    ConfigValue::Static(500)
}

fn default_initial_cursor_behavior() -> ConfigValue<InitialCursorBehaviorDto> {
    ConfigValue::Static(InitialCursorBehaviorDto::StartFromBeginning)
}

fn default_status_filter() -> Option<ConfigValue<String>> {
    Some(ConfigValue::Static("ACTIVE".to_string()))
}

#[derive(OpenApi)]
#[openapi(components(schemas(Open511SourceConfigDto, InitialCursorBehaviorDto)))]
struct Open511SourceSchemas;

/// Descriptor for the Open511 source plugin.
pub struct Open511SourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for Open511SourceDescriptor {
    fn kind(&self) -> &str {
        "open511"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.open511.Open511SourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = Open511SourceSchemas::openapi();
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
        let dto: Open511SourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let initial_cursor_behavior =
            match mapper.resolve_typed::<InitialCursorBehaviorDto>(&dto.initial_cursor_behavior)? {
                InitialCursorBehaviorDto::StartFromBeginning => {
                    InitialCursorBehavior::StartFromBeginning
                }
                InitialCursorBehaviorDto::StartFromNow => InitialCursorBehavior::StartFromNow,
                InitialCursorBehaviorDto::StartFromTimestamp { timestamp_millis } => {
                    InitialCursorBehavior::StartFromTimestamp { timestamp_millis }
                }
            };

        let config = Open511SourceConfig {
            base_url: mapper.resolve_string(&dto.base_url)?,
            poll_interval_secs: mapper.resolve_typed(&dto.poll_interval_secs)?,
            full_sweep_interval: mapper.resolve_typed(&dto.full_sweep_interval)?,
            request_timeout_secs: mapper.resolve_typed(&dto.request_timeout_secs)?,
            page_size: mapper.resolve_typed(&dto.page_size)?,
            status_filter: dto
                .status_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            severity_filter: dto
                .severity_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            event_type_filter: dto
                .event_type_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            area_id_filter: dto
                .area_id_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            road_name_filter: dto
                .road_name_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            jurisdiction_filter: dto
                .jurisdiction_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            bbox_filter: dto
                .bbox_filter
                .as_ref()
                .map(|v| mapper.resolve_string(v))
                .transpose()?,
            auto_delete_archived: mapper.resolve_typed(&dto.auto_delete_archived)?,
            initial_cursor_behavior,
        };

        let source = Open511SourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dto_defaults_status_filter_to_active() {
        let dto: Open511SourceConfigDto = serde_json::from_value(json!({
            "baseUrl": "https://api.open511.gov.bc.ca"
        }))
        .expect("dto should deserialize");

        assert_eq!(
            dto.status_filter,
            Some(ConfigValue::Static("ACTIVE".to_string()))
        );
    }
}
