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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<ConfigValueString>>)]
    pub allowed_event_types: Option<Vec<ConfigValue<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<StatusTransitionDto>>)]
    pub allowed_status_transitions: Option<Vec<StatusTransitionDto>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<ConfigValueString>>)]
    pub allowed_responsibility_types: Option<Vec<ConfigValue<String>>>,
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
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub trusted_router_author_node_ids: Vec<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueU64>)]
    pub trusted_router_author_database_ids: Vec<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_graphql_url: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_rest_url: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_token_env: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub project_status_field_name: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_secs: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub reservation_lease_secs: Option<ConfigValue<u64>>,
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
            allowed_event_types: Some(
                config
                    .allowed_event_types
                    .iter()
                    .cloned()
                    .map(ConfigValue::Static)
                    .collect(),
            ),
            allowed_status_transitions: Some(
                config
                    .allowed_status_transitions
                    .iter()
                    .map(|s| StatusTransitionDto {
                        from: s.from.clone(),
                        to: s.to.clone(),
                    })
                    .collect(),
            ),
            allowed_responsibility_types: Some(
                config
                    .allowed_responsibility_types
                    .iter()
                    .cloned()
                    .map(ConfigValue::Static)
                    .collect(),
            ),
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
            trusted_router_author_node_ids: config
                .trusted_router_author_node_ids
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            trusted_router_author_database_ids: config
                .trusted_router_author_database_ids
                .iter()
                .copied()
                .map(ConfigValue::Static)
                .collect(),
            github_graphql_url: Some(ConfigValue::Static(config.github_graphql_url.clone())),
            github_rest_url: Some(ConfigValue::Static(config.github_rest_url.clone())),
            github_token_env: Some(ConfigValue::Static(config.github_token_env.clone())),
            project_status_field_name: Some(ConfigValue::Static(
                config.project_status_field_name.clone(),
            )),
            timeout_secs: Some(ConfigValue::Static(config.timeout_secs)),
            reservation_lease_secs: Some(ConfigValue::Static(config.reservation_lease_secs)),
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
        let default_config = WorkgraphRouterReactionConfig::default();

        let mut builder = WorkgraphRouterReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_policy_id(mapper.resolve_string(&dto.policy_id).await?)
            .with_policy_type(mapper.resolve_string(&dto.policy_type).await?)
            .with_policy_version(mapper.resolve_string(&dto.policy_version).await?)
            .with_allowed_projects(mapper.resolve_string_vec(&dto.allowed_projects).await?)
            .with_allowed_repos(mapper.resolve_string_vec(&dto.allowed_repos).await?)
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
            .with_trusted_router_author_node_ids(
                mapper
                    .resolve_string_vec(&dto.trusted_router_author_node_ids)
                    .await?,
            )
            .with_github_graphql_url(match dto.github_graphql_url.as_ref() {
                Some(value) => mapper.resolve_string(value).await?,
                None => default_config.github_graphql_url.clone(),
            })
            .with_github_rest_url(match dto.github_rest_url.as_ref() {
                Some(value) => mapper.resolve_string(value).await?,
                None => default_config.github_rest_url.clone(),
            })
            .with_github_token_env(match dto.github_token_env.as_ref() {
                Some(value) => mapper.resolve_string(value).await?,
                None => default_config.github_token_env.clone(),
            });

        if let Some(allowed_event_types) = dto.allowed_event_types.as_ref() {
            builder = builder
                .with_allowed_event_types(mapper.resolve_string_vec(allowed_event_types).await?);
        }

        if let Some(allowed_responsibility_types) = dto.allowed_responsibility_types.as_ref() {
            builder = builder.with_allowed_responsibility_types(
                mapper
                    .resolve_string_vec(allowed_responsibility_types)
                    .await?,
            );
        }

        if !dto.trusted_router_author_database_ids.is_empty() {
            let mut ids = Vec::with_capacity(dto.trusted_router_author_database_ids.len());
            for id in &dto.trusted_router_author_database_ids {
                ids.push(mapper.resolve_typed::<u64>(id).await?);
            }
            builder = builder.with_trusted_router_author_database_ids(ids);
        }

        if let Some(project_status_field_name) = dto.project_status_field_name.as_ref() {
            builder = builder.with_project_status_field_name(
                mapper.resolve_string(project_status_field_name).await?,
            );
        }

        if let Some(timeout_secs) = dto.timeout_secs.as_ref() {
            builder = builder.with_timeout_secs(mapper.resolve_typed::<u64>(timeout_secs).await?);
        }
        if let Some(reservation_lease_secs) = dto.reservation_lease_secs.as_ref() {
            builder = builder.with_reservation_lease_secs(
                mapper.resolve_typed::<u64>(reservation_lease_secs).await?,
            );
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
        if let Some(allowed_status_transitions) = dto.allowed_status_transitions.as_ref() {
            builder = builder.with_allowed_status_transitions(
                allowed_status_transitions
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
    use crate::config::ROUTE_QUERY_ID;
    use serde_json::json;

    #[test]
    fn dto_roundtrip_preserves_policy_fields() {
        let cfg = WorkgraphRouterReactionConfig {
            policy_id: "policy".to_string(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.1".to_string(),
            reservation_lease_secs: 300,
            trusted_router_author_node_ids: vec!["MDQ6VXNlcjE=".to_string()],
            ..WorkgraphRouterReactionConfig::default()
        };
        let dto = WorkgraphRouterReactionConfigDto::from(&cfg);
        assert_eq!(dto.policy_id, ConfigValue::Static("policy".to_string()));
        assert_eq!(dto.policy_version, ConfigValue::Static("1.0.1".to_string()));
        assert_eq!(
            dto.reservation_lease_secs,
            Some(ConfigValue::Static(300)),
            "reservationLeaseSecs must roundtrip through DTO"
        );
        assert_eq!(
            dto.trusted_router_author_node_ids,
            vec![ConfigValue::Static("MDQ6VXNlcjE=".to_string())]
        );
    }

    #[test]
    fn dto_deserialization_with_reservation_lease_secs() {
        let json = json!({
            "policyId": "policy",
            "policyType": "rules_v1",
            "policyVersion": "1.0.0",
            "allowedProjects": [],
            "allowedRepos": [],
            "allowedEventTypes": [],
            "allowedStatusTransitions": [],
            "allowedResponsibilityTypes": [],
            "allowedActors": [],
            "trustedRoutingAuthors": [],
            "trustedLauncherAuthors": [],
            "trustedAgentAuthors": [],
            "trustedRouterAuthors": [],
            "trustedRouterAuthorNodeIds": [],
            "trustedRouterAuthorDatabaseIds": [],
            "githubGraphqlUrl": "https://api.github.com/graphql",
            "githubRestUrl": "https://api.github.com",
            "githubTokenEnv": "GITHUB_TOKEN",
            "reservationLeaseSecs": 300
        });

        let dto: WorkgraphRouterReactionConfigDto =
            serde_json::from_value(json).expect("DTO should parse");
        assert_eq!(dto.reservation_lease_secs, Some(ConfigValue::Static(300)));
    }

    #[test]
    fn dto_deserialization_allows_omitted_github_fields() {
        let json = json!({
            "policyId": "policy",
            "policyType": "rules_v1",
            "policyVersion": "1.0.0",
            "allowedProjects": [],
            "allowedRepos": [],
            "allowedEventTypes": [],
            "allowedStatusTransitions": [],
            "allowedResponsibilityTypes": [],
            "allowedActors": [],
            "trustedRoutingAuthors": [],
            "trustedLauncherAuthors": [],
            "trustedAgentAuthors": [],
            "trustedRouterAuthors": [],
            "trustedRouterAuthorNodeIds": [],
            "trustedRouterAuthorDatabaseIds": []
        });

        let dto: WorkgraphRouterReactionConfigDto =
            serde_json::from_value(json).expect("DTO should parse");
        assert!(dto.github_graphql_url.is_none());
        assert!(dto.github_rest_url.is_none());
        assert!(dto.github_token_env.is_none());
    }

    #[test]
    fn dto_deserialization_rejects_unknown_field() {
        let json = json!({
            "policyId": "policy",
            "policyType": "rules_v1",
            "policyVersion": "1.0.0",
            "allowedProjects": [],
            "allowedRepos": [],
            "allowedEventTypes": [],
            "allowedStatusTransitions": [],
            "allowedResponsibilityTypes": [],
            "allowedActors": [],
            "trustedRoutingAuthors": [],
            "trustedLauncherAuthors": [],
            "trustedAgentAuthors": [],
            "trustedRouterAuthors": [],
            "trustedRouterAuthorNodeIds": [],
            "trustedRouterAuthorDatabaseIds": [],
            "githubGraphqlUrl": "https://api.github.com/graphql",
            "githubRestUrl": "https://api.github.com",
            "githubTokenEnv": "GITHUB_TOKEN",
            "reservationLeaseSecs": 300,
            "totallyUnknownField": true
        });

        let err = serde_json::from_value::<WorkgraphRouterReactionConfigDto>(json)
            .expect_err("unknown fields must be rejected");
        assert!(
            err.to_string().contains("unknown field"),
            "unexpected error: {err}"
        );
    }

    fn minimal_descriptor_json() -> serde_json::Value {
        json!({
            "policyId": "policy",
            "policyType": "rules_v1",
            "policyVersion": "1.0.0",
            "allowedProjects": ["PVT_project"],
            "allowedRepos": ["drasi-project/drasi-core"],
            "allowedActors": ["bot-user"],
            "trustedRoutingAuthors": ["router-user"],
            "trustedLauncherAuthors": ["launcher-user"],
            "trustedAgentAuthors": ["agent-user"],
            "trustedRouterAuthors": ["router-user"],
            "strictRecovery": false
        })
    }

    #[test]
    fn dto_distinguishes_omitted_and_explicit_empty_security_allowlists() {
        let omitted: WorkgraphRouterReactionConfigDto =
            serde_json::from_value(minimal_descriptor_json()).expect("parse omitted fields");
        assert!(omitted.allowed_event_types.is_none());
        assert!(omitted.allowed_status_transitions.is_none());
        assert!(omitted.allowed_responsibility_types.is_none());

        let mut explicit = minimal_descriptor_json();
        explicit["allowedEventTypes"] = json!([]);
        explicit["allowedStatusTransitions"] = json!([]);
        explicit["allowedResponsibilityTypes"] = json!([]);
        let explicit: WorkgraphRouterReactionConfigDto =
            serde_json::from_value(explicit).expect("parse explicit empty fields");
        assert_eq!(explicit.allowed_event_types, Some(vec![]));
        assert_eq!(explicit.allowed_status_transitions, Some(vec![]));
        assert_eq!(explicit.allowed_responsibility_types, Some(vec![]));
    }

    #[tokio::test]
    async fn descriptor_omitted_security_allowlists_use_builder_defaults() {
        let descriptor = WorkgraphRouterReactionDescriptor;
        let created = descriptor
            .create_reaction(
                "router-omitted-defaults",
                vec![ROUTE_QUERY_ID.to_string()],
                &minimal_descriptor_json(),
                true,
            )
            .await;
        assert!(
            created.is_ok(),
            "omitted allowlists should retain defaults and validate"
        );
    }

    #[tokio::test]
    async fn descriptor_rejects_explicit_empty_security_allowlists() {
        let descriptor = WorkgraphRouterReactionDescriptor;

        let mut empty_event_types = minimal_descriptor_json();
        empty_event_types["allowedEventTypes"] = json!([]);
        let err = match descriptor
            .create_reaction(
                "router-empty-events",
                vec![ROUTE_QUERY_ID.to_string()],
                &empty_event_types,
                true,
            )
            .await
        {
            Ok(_) => panic!("explicit empty allowedEventTypes must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("allowedEventTypes must contain at least one entry"),
            "unexpected error: {err:#}"
        );

        let mut empty_transitions = minimal_descriptor_json();
        empty_transitions["allowedStatusTransitions"] = json!([]);
        let err = match descriptor
            .create_reaction(
                "router-empty-transitions",
                vec![ROUTE_QUERY_ID.to_string()],
                &empty_transitions,
                true,
            )
            .await
        {
            Ok(_) => panic!("explicit empty allowedStatusTransitions must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("allowedStatusTransitions must contain at least one transition"),
            "unexpected error: {err:#}"
        );

        let mut empty_responsibilities = minimal_descriptor_json();
        empty_responsibilities["allowedResponsibilityTypes"] = json!([]);
        let err = match descriptor
            .create_reaction(
                "router-empty-responsibilities",
                vec![ROUTE_QUERY_ID.to_string()],
                &empty_responsibilities,
                true,
            )
            .await
        {
            Ok(_) => panic!("explicit empty allowedResponsibilityTypes must fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("allowedResponsibilityTypes must contain at least one entry"),
            "unexpected error: {err:#}"
        );
    }
}
