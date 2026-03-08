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

use crate::{StartPosition, SuiDeepBookSourceBuilder};
use drasi_plugin_sdk::prelude::*;
use std::str::FromStr;
use utoipa::OpenApi;

fn default_rpc_endpoint() -> ConfigValue<String> {
    ConfigValue::Static(crate::config::DEFAULT_SUI_MAINNET_RPC.to_string())
}

fn default_package_id() -> ConfigValue<String> {
    ConfigValue::Static(crate::config::DEFAULT_DEEPBOOK_PACKAGE_ID.to_string())
}

fn default_poll_interval() -> ConfigValue<u64> {
    ConfigValue::Static(2_000)
}

fn default_request_limit() -> ConfigValue<u16> {
    ConfigValue::Static(100)
}

fn default_start_position() -> ConfigValue<StartPositionDto> {
    ConfigValue::Static(StartPositionDto::default())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema, Default)]
#[schema(as = source::sui_deepbook::StartPosition)]
#[serde(rename_all = "snake_case")]
pub enum StartPositionDto {
    Beginning,
    #[default]
    Now,
    Timestamp,
}

impl FromStr for StartPositionDto {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "beginning" => Ok(Self::Beginning),
            "now" => Ok(Self::Now),
            "timestamp" => Ok(Self::Timestamp),
            _ => Err(format!("Invalid start position: {s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::sui_deepbook::SuiDeepBookSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SuiDeepBookSourceConfigDto {
    #[serde(default = "default_rpc_endpoint")]
    #[schema(value_type = ConfigValueString)]
    pub rpc_endpoint: ConfigValue<String>,
    #[serde(default = "default_package_id")]
    #[schema(value_type = ConfigValueString)]
    pub deepbook_package_id: ConfigValue<String>,
    #[serde(default = "default_poll_interval")]
    #[schema(value_type = ConfigValueU64)]
    pub poll_interval_ms: ConfigValue<u64>,
    #[serde(default = "default_request_limit")]
    #[schema(value_type = ConfigValueU16)]
    pub request_limit: ConfigValue<u16>,
    #[serde(default)]
    pub event_filters: Vec<String>,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default = "default_start_position")]
    #[schema(value_type = ConfigValue<source::sui_deepbook::StartPosition>)]
    pub start_position: ConfigValue<StartPositionDto>,
    #[serde(default)]
    #[schema(value_type = Option<ConfigValueString>)]
    pub start_timestamp_ms: Option<ConfigValue<String>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(StartPositionDto, SuiDeepBookSourceConfigDto)))]
struct SuiDeepBookSourceSchemas;

pub struct SuiDeepBookSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for SuiDeepBookSourceDescriptor {
    fn kind(&self) -> &str {
        "sui-deepbook"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.sui_deepbook.SuiDeepBookSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SuiDeepBookSourceSchemas::openapi();
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
        let dto: SuiDeepBookSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let start_position = match mapper.resolve_typed::<StartPositionDto>(&dto.start_position)? {
            StartPositionDto::Beginning => StartPosition::Beginning,
            StartPositionDto::Now => StartPosition::Now,
            StartPositionDto::Timestamp => {
                let ts_config = dto.start_timestamp_ms.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "startTimestampMs is required when startPosition is 'timestamp'"
                    )
                })?;
                let timestamp = mapper
                    .resolve_string(ts_config)?
                    .parse::<i64>()
                    .map_err(|e| anyhow::anyhow!("Invalid startTimestampMs: {e}"))?;
                StartPosition::Timestamp(timestamp)
            }
        };

        let source = SuiDeepBookSourceBuilder::new(id)
            .with_rpc_endpoint(mapper.resolve_string(&dto.rpc_endpoint)?)
            .with_deepbook_package_id(mapper.resolve_string(&dto.deepbook_package_id)?)
            .with_poll_interval_ms(mapper.resolve_typed(&dto.poll_interval_ms)?)
            .with_request_limit(mapper.resolve_typed(&dto.request_limit)?)
            .with_event_filters(dto.event_filters.clone())
            .with_pools(dto.pools.clone())
            .with_start_position(start_position)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
