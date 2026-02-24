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

//! Descriptor for the platform reaction plugin.

use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;
use drasi_lib::reactions::Reaction;

use crate::PlatformReactionBuilder;

/// Configuration DTO for the platform reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = PlatformReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct PlatformReactionConfigDto {
    /// Redis connection URL.
    #[schema(value_type = ConfigValueString)]
    pub redis_url: ConfigValue<String>,

    /// PubSub name/topic.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub pubsub_name: Option<ConfigValue<String>>,

    /// Source name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub source_name: Option<ConfigValue<String>>,

    /// Maximum stream length.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub max_stream_length: Option<ConfigValue<usize>>,

    /// Whether to emit control events.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub emit_control_events: Option<ConfigValue<bool>>,

    /// Whether batching is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub batch_enabled: Option<ConfigValue<bool>>,

    /// Maximum batch size.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub batch_max_size: Option<ConfigValue<usize>>,

    /// Maximum batch wait time in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub batch_max_wait_ms: Option<ConfigValue<u64>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(PlatformReactionConfigDto)))]
struct PlatformReactionSchemas;

/// Descriptor for the platform reaction plugin.
pub struct PlatformReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for PlatformReactionDescriptor {
    fn kind(&self) -> &str {
        "platform"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "PlatformReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = PlatformReactionSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: PlatformReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = PlatformReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_redis_url(mapper.resolve_string(&dto.redis_url)?);

        if let Some(ref v) = dto.pubsub_name {
            builder = builder.with_pubsub_name(mapper.resolve_string(v)?);
        }
        if let Some(ref v) = dto.source_name {
            builder = builder.with_source_name(mapper.resolve_string(v)?);
        }
        if let Some(ref v) = dto.max_stream_length {
            builder = builder.with_max_stream_length(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.emit_control_events {
            builder = builder.with_emit_control_events(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.batch_enabled {
            builder = builder.with_batch_enabled(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.batch_max_size {
            builder = builder.with_batch_max_size(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.batch_max_wait_ms {
            builder = builder.with_batch_max_wait_ms(mapper.resolve_typed(v)?);
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
