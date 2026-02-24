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

//! Platform source plugin descriptor and configuration DTOs.

use crate::{PlatformSourceBuilder, PlatformSourceConfig};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Platform source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = PlatformSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlatformSourceConfigDto {
    pub redis_url: ConfigValue<String>,
    pub stream_key: ConfigValue<String>,
    #[serde(default = "default_consumer_group")]
    pub consumer_group: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_name: Option<ConfigValue<String>>,
    #[serde(default = "default_batch_size")]
    pub batch_size: ConfigValue<usize>,
    #[serde(default = "default_block_ms")]
    pub block_ms: ConfigValue<u64>,
}

fn default_consumer_group() -> ConfigValue<String> {
    ConfigValue::Static("drasi-core".to_string())
}

fn default_batch_size() -> ConfigValue<usize> {
    ConfigValue::Static(100)
}

fn default_block_ms() -> ConfigValue<u64> {
    ConfigValue::Static(5000)
}

#[derive(OpenApi)]
#[openapi(components(schemas(PlatformSourceConfigDto)))]
struct PlatformSourceSchemas;

/// Descriptor for the platform source plugin.
pub struct PlatformSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for PlatformSourceDescriptor {
    fn kind(&self) -> &str {
        "platform"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "PlatformSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = PlatformSourceSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: PlatformSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = PlatformSourceConfig {
            redis_url: mapper.resolve_string(&dto.redis_url)?,
            stream_key: mapper.resolve_string(&dto.stream_key)?,
            consumer_group: mapper.resolve_string(&dto.consumer_group)?,
            consumer_name: mapper.resolve_optional(&dto.consumer_name)?,
            batch_size: mapper.resolve_typed(&dto.batch_size)?,
            block_ms: mapper.resolve_typed(&dto.block_ms)?,
        };

        let source = PlatformSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
