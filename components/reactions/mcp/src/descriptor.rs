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

//! Descriptor for the MCP reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::{McpReactionBuilder, NotificationTemplate, QueryConfig};

/// DTO for MCP notification template.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mcp::NotificationTemplate)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NotificationTemplateDto {
    /// Handlebars template string.
    pub template: String,
}

/// DTO for per-query MCP configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mcp::McpQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpQueryConfigDto {
    /// Resource title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Resource description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Template for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<NotificationTemplateDto>,

    /// Template for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<NotificationTemplateDto>,

    /// Template for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<NotificationTemplateDto>,
}

/// Configuration DTO for the MCP reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mcp::McpReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct McpReactionConfigDto {
    /// HTTP server port.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU16>)]
    pub port: Option<ConfigValue<u16>>,

    /// Optional bearer token for authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub bearer_token: Option<ConfigValue<String>>,

    /// Query-specific configurations.
    #[serde(default)]
    pub routes: HashMap<String, McpQueryConfigDto>,
}

fn map_template(dto: &NotificationTemplateDto) -> NotificationTemplate {
    NotificationTemplate {
        template: dto.template.clone(),
    }
}

fn map_query_config(dto: &McpQueryConfigDto) -> QueryConfig {
    QueryConfig {
        title: dto.title.clone(),
        description: dto.description.clone(),
        added: dto.added.as_ref().map(map_template),
        updated: dto.updated.as_ref().map(map_template),
        deleted: dto.deleted.as_ref().map(map_template),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(McpReactionConfigDto, McpQueryConfigDto, NotificationTemplateDto,)))]
struct McpReactionSchemas;

/// Descriptor for the MCP reaction plugin.
pub struct McpReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for McpReactionDescriptor {
    fn kind(&self) -> &str {
        "mcp"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.mcp.McpReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = McpReactionSchemas::openapi();
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
        let dto: McpReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = McpReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref port) = dto.port {
            builder = builder.with_port(mapper.resolve_typed(port)?);
        }

        if let Some(ref token) = dto.bearer_token {
            builder = builder.with_bearer_token(mapper.resolve_string(token)?);
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_value_resolution() {
        std::env::set_var("MCP_TEST_PORT", "3456");
        let dto = McpReactionConfigDto {
            port: Some(ConfigValue::EnvironmentVariable {
                name: "MCP_TEST_PORT".to_string(),
                default: None,
            }),
            bearer_token: Some(ConfigValue::Static("token".to_string())),
            routes: HashMap::new(),
        };

        let mapper = DtoMapper::new();
        let port = mapper.resolve_typed(dto.port.as_ref().unwrap()).unwrap();
        let token = mapper
            .resolve_string(dto.bearer_token.as_ref().unwrap())
            .unwrap();

        assert_eq!(port, 3456);
        assert_eq!(token, "token");
    }
}
