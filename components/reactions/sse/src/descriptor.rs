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

//! Descriptor for the SSE reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::config::SseExtension;
use crate::SseReactionBuilder;

/// DTO for an SSE template specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SseTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SseTemplateSpecDto {
    /// Handlebars template string.
    #[serde(default)]
    pub template: String,

    /// Optional custom path for this template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// DTO for per-query SSE template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SseQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SseQueryConfigDto {
    /// Template for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<SseTemplateSpecDto>,

    /// Template for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<SseTemplateSpecDto>,

    /// Template for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<SseTemplateSpecDto>,
}

/// Configuration DTO for the SSE reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = SseReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct SseReactionConfigDto {
    /// Host to bind SSE server.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub host: Option<ConfigValue<String>>,

    /// Port to bind SSE server.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU16>)]
    pub port: Option<ConfigValue<u16>>,

    /// SSE path.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub sse_path: Option<ConfigValue<String>>,

    /// Heartbeat interval in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub heartbeat_interval_ms: Option<ConfigValue<u64>>,

    /// Query-specific template configurations.
    #[serde(default)]
    pub routes: HashMap<String, SseQueryConfigDto>,

    /// Default template configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<SseQueryConfigDto>,
}

fn map_template_spec(dto: &SseTemplateSpecDto) -> crate::TemplateSpec {
    crate::TemplateSpec {
        template: dto.template.clone(),
        extension: SseExtension {
            path: dto.path.clone(),
        },
    }
}

fn map_query_config(dto: &SseQueryConfigDto) -> crate::QueryConfig {
    crate::QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(SseReactionConfigDto, SseQueryConfigDto, SseTemplateSpecDto,)))]
struct SseReactionSchemas;

/// Descriptor for the SSE reaction plugin.
pub struct SseReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for SseReactionDescriptor {
    fn kind(&self) -> &str {
        "sse"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "SseReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = SseReactionSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: SseReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = SseReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref host) = dto.host {
            builder = builder.with_host(mapper.resolve_string(host)?);
        }
        if let Some(ref port) = dto.port {
            builder = builder.with_port(mapper.resolve_typed(port)?);
        }
        if let Some(ref sse_path) = dto.sse_path {
            builder = builder.with_sse_path(mapper.resolve_string(sse_path)?);
        }
        if let Some(ref heartbeat) = dto.heartbeat_interval_ms {
            builder = builder.with_heartbeat_interval_ms(mapper.resolve_typed(heartbeat)?);
        }

        if let Some(ref default_template) = dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
