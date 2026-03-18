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
    /// HTTP server bind address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub host: Option<ConfigValue<String>>,

    /// HTTP server port.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU16>)]
    pub port: Option<ConfigValue<u16>>,

    /// Optional bearer token for authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub bearer_token: Option<ConfigValue<String>>,

    /// Maximum number of concurrent sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,

    /// Per-session notification channel capacity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_channel_capacity: Option<usize>,

    /// Query-specific configurations.
    ///
    /// Each entry maps a query ID to its MCP configuration. Accepts both a
    /// single object and a single-element array per key (for YAML friendliness).
    #[serde(default, deserialize_with = "deserialize_routes")]
    pub routes: HashMap<String, McpQueryConfigDto>,
}

/// Deserialize the `routes` map, tolerating single-element arrays as values.
///
/// In YAML, users sometimes write:
/// ```yaml
/// routes:
///   my-query:
///     - title: My Query    # ← dash creates an array
/// ```
/// instead of:
/// ```yaml
/// routes:
///   my-query:
///     title: My Query      # ← correct map syntax
/// ```
///
/// This deserializer accepts both forms: a bare object or a single-element array
/// containing an object.
fn deserialize_routes<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, McpQueryConfigDto>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let raw: HashMap<String, serde_json::Value> = HashMap::deserialize(deserializer)?;
    let mut result = HashMap::with_capacity(raw.len());

    for (key, value) in raw {
        let dto = match &value {
            // Single-element array → unwrap and deserialize the element
            serde_json::Value::Array(arr) if arr.len() == 1 => {
                serde_json::from_value::<McpQueryConfigDto>(arr[0].clone()).map_err(|e| {
                    D::Error::custom(format!(
                        "invalid route config for '{key}' (inside single-element array): {e}"
                    ))
                })?
            }
            // Empty or multi-element array → clear error
            serde_json::Value::Array(arr) => {
                return Err(D::Error::custom(format!(
                    "route '{key}' must be an object, not an array of {} elements",
                    arr.len()
                )));
            }
            // Normal object → deserialize directly
            _ => serde_json::from_value::<McpQueryConfigDto>(value)
                .map_err(|e| D::Error::custom(format!("invalid route config for '{key}': {e}")))?,
        };
        result.insert(key, dto);
    }

    Ok(result)
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
        log::info!("[{id}] Creating MCP reaction (queries: {query_ids:?}, auto_start: {auto_start})");
        log::debug!("[{id}] Raw config JSON: {config_json}");

        let dto: McpReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = McpReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref host) = dto.host {
            let resolved = mapper.resolve_string(host)?;
            log::debug!("[{id}] Resolved host: {resolved}");
            builder = builder.with_host(resolved);
        }

        if let Some(ref port) = dto.port {
            let resolved = mapper.resolve_typed(port)?;
            log::debug!("[{id}] Resolved port: {resolved}");
            builder = builder.with_port(resolved);
        }

        if let Some(ref token) = dto.bearer_token {
            let resolved = mapper.resolve_string(token)?;
            log::debug!("[{id}] Bearer token configured ({} chars)", resolved.len());
            builder = builder.with_bearer_token(resolved);
        }

        if let Some(max_sessions) = dto.max_sessions {
            log::debug!("[{id}] Max sessions: {max_sessions}");
            builder = builder.with_max_sessions(max_sessions);
        }

        if let Some(capacity) = dto.session_channel_capacity {
            log::debug!("[{id}] Session channel capacity: {capacity}");
            builder = builder.with_session_channel_capacity(capacity);
        }

        for (query_id, config) in &dto.routes {
            log::debug!(
                "[{id}] Route '{query_id}': title={:?}, has_added={}, has_updated={}, has_deleted={}",
                config.title,
                config.added.is_some(),
                config.updated.is_some(),
                config.deleted.is_some()
            );
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        log::info!("[{id}] MCP reaction created successfully");
        Ok(Box::new(reaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_value_resolution() {
        let env_var = format!("MCP_TEST_PORT_{}", std::process::id());
        std::env::set_var(&env_var, "3456");
        let dto = McpReactionConfigDto {
            port: Some(ConfigValue::EnvironmentVariable {
                name: env_var.clone(),
                default: None,
            }),
            bearer_token: Some(ConfigValue::Static("token".to_string())),
            host: None,
            max_sessions: None,
            session_channel_capacity: None,
            routes: HashMap::new(),
        };

        let mapper = DtoMapper::new();
        let port = mapper.resolve_typed(dto.port.as_ref().unwrap()).unwrap();
        let token = mapper
            .resolve_string(dto.bearer_token.as_ref().unwrap())
            .unwrap();

        assert_eq!(port, 3456);
        assert_eq!(token, "token");

        std::env::remove_var(&env_var);
    }

    #[test]
    fn test_routes_as_map() {
        let json = serde_json::json!({
            "port": 8083,
            "routes": {
                "query1": {
                    "title": "My Query",
                    "added": {"template": "added {{after.name}}"},
                    "deleted": {"template": "deleted {{before.name}}"}
                }
            }
        });
        let dto: McpReactionConfigDto = serde_json::from_value(json).expect("should parse");
        let route = dto.routes.get("query1").expect("route exists");
        assert_eq!(route.title.as_deref(), Some("My Query"));
        assert_eq!(
            route.added.as_ref().unwrap().template,
            "added {{after.name}}"
        );
    }

    #[test]
    fn test_routes_as_single_element_array() {
        let json = serde_json::json!({
            "port": 8083,
            "routes": {
                "query1": [{
                    "title": "My Query",
                    "added": {"template": "added {{after.name}}"},
                    "deleted": {"template": "deleted {{before.name}}"}
                }]
            }
        });
        let dto: McpReactionConfigDto = serde_json::from_value(json).expect("should parse");
        let route = dto.routes.get("query1").expect("route exists");
        assert_eq!(route.title.as_deref(), Some("My Query"));
        assert_eq!(
            route.added.as_ref().unwrap().template,
            "added {{after.name}}"
        );
    }

    #[test]
    fn test_routes_empty_array_rejected() {
        let json = serde_json::json!({
            "routes": {
                "query1": []
            }
        });
        let result: Result<McpReactionConfigDto, _> = serde_json::from_value(json);
        let err = result.expect_err("should fail");
        assert!(
            err.to_string().contains("not an array of 0 elements"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_routes_multi_element_array_rejected() {
        let json = serde_json::json!({
            "routes": {
                "query1": [
                    {"title": "A"},
                    {"title": "B"}
                ]
            }
        });
        let result: Result<McpReactionConfigDto, _> = serde_json::from_value(json);
        let err = result.expect_err("should fail");
        assert!(
            err.to_string().contains("not an array of 2 elements"),
            "unexpected error: {err}"
        );
    }
}
