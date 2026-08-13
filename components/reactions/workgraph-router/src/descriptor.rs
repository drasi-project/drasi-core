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

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{StatusTransition, WorkgraphRouterReactionConfig};
use crate::{WorkgraphRouterReaction, WorkgraphRouterReactionBuilder};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::workgraph_router::StatusTransitionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StatusTransitionDto {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::workgraph_router::WorkgraphRouterReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkgraphRouterReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub policy_id: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub policy_type: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub policy_version: ConfigValue<String>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub allowed_projects: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub allowed_repos: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub allowed_event_types: Vec<ConfigValue<String>>,
    #[serde(default)]
    pub allowed_status_transitions: Vec<StatusTransitionDto>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub allowed_responsibility_types: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub allowed_actors: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub trusted_routing_authors: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub trusted_launcher_authors: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub trusted_agent_authors: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub trusted_router_authors: Vec<ConfigValue<String>>,
    #[schema(value_type = ConfigValueString)]
    pub github_graphql_url: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub github_rest_url: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub github_token_env: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub project_status_field_name: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_secs: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub strict_recovery: Option<ConfigValue<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,
}

impl From<&WorkgraphRouterReactionConfig> for WorkgraphRouterReactionConfigDto {
    fn from(config: &WorkgraphRouterReactionConfig) -> Self {
        Self {
            policy_id: ConfigValue::Static(config.policy_id.clone()),
            policy_type: ConfigValue::Static(config.policy_type.clone()),
            policy_version: ConfigValue::Static(config.policy_version.clone()),
            allowed_projects: config
                .allowed_projects
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            allowed_repos: config
                .allowed_repos
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            allowed_event_types: config
                .allowed_event_types
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            allowed_status_transitions: config
                .allowed_status_transitions
                .iter()
                .map(|s| StatusTransitionDto {
                    from: s.from.clone(),
                    to: s.to.clone(),
                })
                .collect(),
            allowed_responsibility_types: config
                .allowed_responsibility_types
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            allowed_actors: config
                .allowed_actors
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            trusted_routing_authors: config
                .trusted_routing_authors
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            trusted_launcher_authors: config
                .trusted_launcher_authors
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            trusted_agent_authors: config
                .trusted_agent_authors
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            trusted_router_authors: config
                .trusted_router_authors
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            github_graphql_url: ConfigValue::Static(config.github_graphql_url.clone()),
            github_rest_url: ConfigValue::Static(config.github_rest_url.clone()),
            github_token_env: ConfigValue::Static(config.github_token_env.clone()),
            project_status_field_name: Some(ConfigValue::Static(
                config.project_status_field_name.clone(),
            )),
            timeout_secs: Some(ConfigValue::Static(config.timeout_secs)),
            strict_recovery: Some(ConfigValue::Static(config.strict_recovery)),
            priority_queue_capacity: None,
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    WorkgraphRouterReactionConfigDto,
    StatusTransitionDto,
    StatusTransition,
)))]
struct WorkgraphRouterReactionSchemas;

pub struct WorkgraphRouterReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for WorkgraphRouterReactionDescriptor {
    fn kind(&self) -> &str {
        "workgraph-router"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.workgraph_router.WorkgraphRouterReactionConfig"
    }

    fn display_name(&self) -> &str {
        "WorkGraph Router"
    }

    fn display_description(&self) -> &str {
        "Applies deterministic WorkGraph routing policy decisions to GitHub project items."
    }

    fn display_icon(&self) -> &str {
        "github"
    }

    fn config_schema_json(&self) -> String {
        let api = WorkgraphRouterReactionSchemas::openapi();
        api.components
            .as_ref()
            .and_then(|c| serde_json::to_string(&c.schemas).ok())
            .unwrap_or_else(|| "{}".to_string())
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: WorkgraphRouterReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = WorkgraphRouterReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_policy_id(mapper.resolve_string(&dto.policy_id).await?)
            .with_policy_type(mapper.resolve_string(&dto.policy_type).await?)
            .with_policy_version(mapper.resolve_string(&dto.policy_version).await?)
            .with_allowed_projects(mapper.resolve_string_vec(&dto.allowed_projects).await?)
            .with_allowed_repos(mapper.resolve_string_vec(&dto.allowed_repos).await?)
            .with_allowed_event_types(mapper.resolve_string_vec(&dto.allowed_event_types).await?)
            .with_allowed_responsibility_types(
                mapper
                    .resolve_string_vec(&dto.allowed_responsibility_types)
                    .await?,
            )
            .with_allowed_actors(mapper.resolve_string_vec(&dto.allowed_actors).await?)
            .with_trusted_routing_authors(
                mapper
                    .resolve_string_vec(&dto.trusted_routing_authors)
                    .await?,
            )
            .with_trusted_launcher_authors(
                mapper
                    .resolve_string_vec(&dto.trusted_launcher_authors)
                    .await?,
            )
            .with_trusted_agent_authors(
                mapper
                    .resolve_string_vec(&dto.trusted_agent_authors)
                    .await?,
            )
            .with_trusted_router_authors(
                mapper
                    .resolve_string_vec(&dto.trusted_router_authors)
                    .await?,
            )
            .with_github_graphql_url(mapper.resolve_string(&dto.github_graphql_url).await?)
            .with_github_rest_url(mapper.resolve_string(&dto.github_rest_url).await?)
            .with_github_token_env(mapper.resolve_string(&dto.github_token_env).await?);

        if let Some(project_status_field_name) = dto.project_status_field_name.as_ref() {
            builder = builder.with_project_status_field_name(
                mapper.resolve_string(project_status_field_name).await?,
            );
        }

        if let Some(timeout_secs) = dto.timeout_secs.as_ref() {
            builder = builder.with_timeout_secs(mapper.resolve_typed::<u64>(timeout_secs).await?);
        }
        if let Some(strict_recovery) = dto.strict_recovery.as_ref() {
            builder =
                builder.with_strict_recovery(mapper.resolve_typed::<bool>(strict_recovery).await?);
        }
        if let Some(priority_queue_capacity) = dto.priority_queue_capacity.as_ref() {
            builder = builder.with_priority_queue_capacity(
                mapper.resolve_typed::<u64>(priority_queue_capacity).await? as usize,
            );
        }
        if !dto.allowed_status_transitions.is_empty() {
            builder = builder.with_allowed_status_transitions(
                dto.allowed_status_transitions
                    .iter()
                    .map(|item| StatusTransition {
                        from: item.from.clone(),
                        to: item.to.clone(),
                    })
                    .collect(),
            );
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());
        Ok(Box::new(reaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dto_roundtrip_preserves_policy_fields() {
        let cfg = WorkgraphRouterReactionConfig {
            policy_id: "policy".to_string(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.1".to_string(),
            ..WorkgraphRouterReactionConfig::default()
        };
        let dto = WorkgraphRouterReactionConfigDto::from(&cfg);
        assert_eq!(dto.policy_id, ConfigValue::Static("policy".to_string()));
        assert_eq!(dto.policy_version, ConfigValue::Static("1.0.1".to_string()));
    }
}
