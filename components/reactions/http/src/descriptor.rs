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

//! Descriptor for the HTTP reaction plugin.

use drasi_plugin_sdk::prelude::*;
use drasi_lib::reactions::Reaction;
use std::collections::HashMap;

use crate::HttpReactionBuilder;

/// DTO for an HTTP call specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
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

/// Configuration DTO for the HTTP reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HttpReactionConfigDto {
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
}

fn map_call_spec(dto: &CallSpecDto) -> crate::CallSpec {
    crate::CallSpec {
        url: dto.url.clone(),
        method: dto.method.clone(),
        body: dto.body.clone(),
        headers: dto.headers.clone(),
    }
}

fn map_query_config(dto: &HttpQueryConfigDto) -> crate::QueryConfig {
    crate::QueryConfig {
        added: dto.added.as_ref().map(map_call_spec),
        updated: dto.updated.as_ref().map(map_call_spec),
        deleted: dto.deleted.as_ref().map(map_call_spec),
    }
}

/// Descriptor for the HTTP reaction plugin.
pub struct HttpReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for HttpReactionDescriptor {
    fn kind(&self) -> &str {
        "http"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_json(&self) -> String {
        let schema = <HttpReactionConfigDto as utoipa::ToSchema>::schema();
        serde_json::to_string(&schema).unwrap()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: HttpReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = HttpReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_base_url(mapper.resolve_string(&dto.base_url)?);

        if let Some(ref token) = dto.token {
            builder = builder.with_token(mapper.resolve_string(token)?);
        }

        if let Some(ref timeout_ms) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(timeout_ms)?);
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
