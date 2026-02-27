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

//! Descriptor for the HTTP adaptive reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::config::{CallSpec, QueryConfig};
use crate::HttpAdaptiveReactionBuilder;

/// DTO for an HTTP call specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = CallSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CallSpecDto {
    /// URL path or absolute URL (supports Handlebars templates).
    pub url: String,

    /// HTTP method (GET, POST, PUT, DELETE, PATCH).
    pub method: String,

    /// Request body as a Handlebars template.
    #[serde(default)]
    pub body: String,

    /// Additional HTTP headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// DTO for per-query HTTP call configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = HttpQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpQueryConfigDto {
    /// HTTP call specification for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<CallSpecDto>,

    /// HTTP call specification for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<CallSpecDto>,

    /// HTTP call specification for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<CallSpecDto>,
}

/// Configuration DTO for the HTTP adaptive reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = HttpAdaptiveReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct HttpAdaptiveReactionConfigDto {
    /// Base URL for HTTP requests.
    #[schema(value_type = ConfigValueString)]
    pub base_url: ConfigValue<String>,

    /// Optional authentication token.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub token: Option<ConfigValue<String>>,

    /// Request timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    /// Query-specific call configurations.
    #[serde(default)]
    pub routes: HashMap<String, HttpQueryConfigDto>,

    /// Minimum adaptive batch size.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub adaptive_min_batch_size: Option<ConfigValue<usize>>,

    /// Maximum adaptive batch size.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub adaptive_max_batch_size: Option<ConfigValue<usize>>,

    /// Adaptive window size in 100ms units.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueUsize>)]
    pub adaptive_window_size: Option<ConfigValue<usize>>,

    /// Adaptive batch timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub adaptive_batch_timeout_ms: Option<ConfigValue<u64>>,
}

fn map_call_spec(dto: &CallSpecDto) -> CallSpec {
    CallSpec {
        url: dto.url.clone(),
        method: dto.method.clone(),
        body: dto.body.clone(),
        headers: dto.headers.clone(),
    }
}

fn map_query_config(dto: &HttpQueryConfigDto) -> QueryConfig {
    QueryConfig {
        added: dto.added.as_ref().map(map_call_spec),
        updated: dto.updated.as_ref().map(map_call_spec),
        deleted: dto.deleted.as_ref().map(map_call_spec),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(HttpAdaptiveReactionConfigDto)))]
struct HttpAdaptiveReactionSchemas;

/// Descriptor for the HTTP adaptive reaction plugin.
pub struct HttpAdaptiveReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for HttpAdaptiveReactionDescriptor {
    fn kind(&self) -> &str {
        "http-adaptive"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "HttpAdaptiveReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = HttpAdaptiveReactionSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: HttpAdaptiveReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = HttpAdaptiveReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_base_url(mapper.resolve_string(&dto.base_url)?);

        if let Some(ref token) = dto.token {
            builder = builder.with_token(mapper.resolve_string(token)?);
        }

        if let Some(ref timeout_ms) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(timeout_ms)?);
        }

        if let Some(ref v) = dto.adaptive_min_batch_size {
            builder = builder.with_min_batch_size(mapper.resolve_typed(v)?);
        }

        if let Some(ref v) = dto.adaptive_max_batch_size {
            builder = builder.with_max_batch_size(mapper.resolve_typed(v)?);
        }

        if let Some(ref v) = dto.adaptive_window_size {
            builder = builder.with_window_size(mapper.resolve_typed(v)?);
        }

        if let Some(ref v) = dto.adaptive_batch_timeout_ms {
            builder = builder.with_batch_timeout_ms(mapper.resolve_typed(v)?);
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
