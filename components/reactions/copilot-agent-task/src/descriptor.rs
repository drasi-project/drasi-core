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

//! Descriptor for the Copilot Agent Task reaction plugin.

use std::collections::HashMap;

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use drasi_workgraph_common::trust::ActorType;

use crate::config::{CommentApiConfig, CopilotAgentTaskReactionConfig};
use crate::CopilotAgentTaskReactionBuilder;

/// DTO mirroring [`CommentApiConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::copilot_agent_task::CommentApiConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommentApiConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub max_attempts: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub retry_backoff_ms: Option<ConfigValue<u64>>,
}

/// Top-level Copilot Agent Task reaction config DTO.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::copilot_agent_task::CopilotAgentTaskReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CopilotAgentTaskReactionConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_api_base_url: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_graphql_url: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub agent_tasks_api_version: Option<ConfigValue<String>>,

    /// A fine-grained PAT or GitHub App user token. Resolved from an
    /// environment variable or secret store — never a literal in
    /// declarative config. See [`crate::config::CopilotAgentTaskReactionConfig::token`].
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValue<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub expected_github_user_id: Option<ConfigValue<String>>,

    pub allowed_repositories: Vec<String>,
    pub allowed_profiles: Vec<String>,
    pub allowed_models: Vec<String>,

    /// Numeric GitHub database ID of the identity whose
    /// `ResponsibilityAssigned` comments are trusted (the assigning reaction's
    /// identity).
    /// Together with `trustedAssignmentAuthorType` this is the whole trust key:
    /// no node ID and no GitHub App attribution is configured or accepted.
    #[schema(value_type = ConfigValueU64)]
    pub trusted_assignment_author_database_id: ConfigValue<u64>,

    /// Actor type of the identity whose `ResponsibilityAssigned` comments are
    /// trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_assignment_author_type: Option<ActorType>,

    /// Numeric GitHub database ID of the identity **this** reaction posts as,
    /// used only to adopt its own `ExecutionStarted` comment after an ambiguous
    /// write. It must name the account `token` authenticates as.
    #[schema(value_type = ConfigValueU64)]
    pub trusted_execution_author_database_id: ConfigValue<u64>,

    /// Actor type of the identity this reaction posts as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_execution_author_type: Option<ActorType>,

    /// The node ID of the Project's single-select `Status` field
    /// (`PVTSSF_…`), pinned so a renamed/rebuilt field is rejected.
    #[schema(value_type = ConfigValueString)]
    pub expected_project_status_field_node_id: ConfigValue<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub request_timeout_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_api: Option<CommentApiConfigDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub strict_recovery: Option<ConfigValue<bool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,
}

impl From<&CopilotAgentTaskReactionConfig> for CopilotAgentTaskReactionConfigDto {
    fn from(c: &CopilotAgentTaskReactionConfig) -> Self {
        Self {
            github_api_base_url: Some(ConfigValue::Static(c.github_api_base_url.clone())),
            github_graphql_url: Some(ConfigValue::Static(c.github_graphql_url.clone())),
            agent_tasks_api_version: Some(ConfigValue::Static(c.agent_tasks_api_version.clone())),
            token: ConfigValue::Static(c.token.clone()),
            expected_github_user_id: c.expected_github_user_id.clone().map(ConfigValue::Static),
            allowed_repositories: c.allowed_repositories.clone(),
            allowed_profiles: c.allowed_profiles.clone(),
            allowed_models: c.allowed_models.clone(),
            trusted_assignment_author_database_id: ConfigValue::Static(
                c.trusted_assignment_author_database_id,
            ),
            trusted_assignment_author_type: Some(c.trusted_assignment_author_type),
            trusted_execution_author_database_id: ConfigValue::Static(
                c.trusted_execution_author_database_id,
            ),
            trusted_execution_author_type: Some(c.trusted_execution_author_type),
            expected_project_status_field_node_id: ConfigValue::Static(
                c.expected_project_status_field_node_id.clone(),
            ),
            request_timeout_ms: Some(ConfigValue::Static(c.request_timeout_ms)),
            comment_api: Some(CommentApiConfigDto {
                max_attempts: Some(ConfigValue::Static(c.comment_api.max_attempts as u64)),
                retry_backoff_ms: Some(ConfigValue::Static(c.comment_api.retry_backoff_ms)),
            }),
            strict_recovery: Some(ConfigValue::Static(c.strict_recovery)),
            priority_queue_capacity: None,
        }
    }
}

async fn map_comment_api(
    dto: &CommentApiConfigDto,
    mapper: &DtoMapper,
) -> anyhow::Result<CommentApiConfig> {
    let default = CommentApiConfig::default();
    Ok(CommentApiConfig {
        max_attempts: match &dto.max_attempts {
            Some(v) => {
                let resolved: u64 = mapper.resolve_typed(v).await?;
                u32::try_from(resolved)
                    .map_err(|_| anyhow::anyhow!("`commentApi.maxAttempts` exceeds u32"))?
            }
            None => default.max_attempts,
        },
        retry_backoff_ms: match &dto.retry_backoff_ms {
            Some(v) => mapper.resolve_typed(v).await?,
            None => default.retry_backoff_ms,
        },
    })
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    CopilotAgentTaskReactionConfigDto,
    CommentApiConfigDto,
    ActorType,
    ConfigValueStringSchema,
    ConfigValueU64Schema,
    ConfigValueBoolSchema,
)))]
struct CopilotAgentTaskReactionSchemas;

/// Descriptor for the Copilot Agent Task reaction plugin.
pub struct CopilotAgentTaskReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for CopilotAgentTaskReactionDescriptor {
    fn kind(&self) -> &str {
        "copilot-agent-task"
    }

    fn config_version(&self) -> &str {
        "2.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.copilot_agent_task.CopilotAgentTaskReactionConfig"
    }

    fn display_name(&self) -> &str {
        "Copilot Agent Task"
    }

    fn display_description(&self) -> &str {
        "Launches a GitHub Copilot coding-agent task for a nominated WorkGraph run: re-reads the trusted issue, project item, and assignment from GitHub, pins the exact agent-profile blob, durably reserves and creates or adopts exactly one task, and posts exactly one shared ExecutionStarted WorkGraphEvent/v1 issue comment."
    }

    fn display_icon(&self) -> &str {
        "github"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = CopilotAgentTaskReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");

        SchemaUiAnnotator::new(
            schemas,
            "reaction.copilot_agent_task.CopilotAgentTaskReactionConfig",
        )
        .expect("root schema not found")
        .field("githubApiBaseUrl", |f| {
            f.group("GitHub")
                .order(1)
                .placeholder("https://api.github.com")
        })
        .field("githubGraphqlUrl", |f| {
            f.group("GitHub")
                .order(2)
                .placeholder("https://api.github.com/graphql")
        })
        .field("agentTasksApiVersion", |f| f.group("GitHub").order(3))
        .field("token", |f| f.group("GitHub").order(4).widget("password"))
        .field("expectedGithubUserId", |f| f.group("GitHub").order(5))
        .field("allowedRepositories", |f| f.group("Allowlists").order(10))
        .field("allowedProfiles", |f| f.group("Allowlists").order(11))
        .field("allowedModels", |f| f.group("Allowlists").order(12))
        .field("trustedAssignmentAuthorDatabaseId", |f| {
            f.group("Allowlists").order(13).placeholder("4021243")
        })
        .field("trustedAssignmentAuthorType", |f| {
            f.group("Allowlists").order(14)
        })
        .field("trustedExecutionAuthorDatabaseId", |f| {
            f.group("Allowlists").order(15).placeholder("90210")
        })
        .field("trustedExecutionAuthorType", |f| {
            f.group("Allowlists").order(16)
        })
        .field("expectedProjectStatusFieldNodeId", |f| {
            f.group("Project").order(15).placeholder("PVTSSF_...")
        })
        .field("requestTimeoutMs", |f| {
            f.group("Advanced").order(20).placeholder("30000")
        })
        .field("commentApi", |f| f.group("Comment API").order(30))
        .field("strictRecovery", |f| f.group("Advanced").order(21))
        .field("priorityQueueCapacity", |f| {
            f.group("Advanced").order(22).placeholder("10000")
        })
        .annotate()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: CopilotAgentTaskReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = CopilotAgentTaskReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_token(mapper.resolve_string(&dto.token).await?);

        if let Some(ref v) = dto.github_api_base_url {
            builder = builder.with_github_api_base_url(mapper.resolve_string(v).await?);
        }
        if let Some(ref v) = dto.github_graphql_url {
            builder = builder.with_github_graphql_url(mapper.resolve_string(v).await?);
        }
        if let Some(ref v) = dto.agent_tasks_api_version {
            builder = builder.with_agent_tasks_api_version(mapper.resolve_string(v).await?);
        }
        if let Some(ref v) = dto.expected_github_user_id {
            builder = builder.with_expected_github_user_id(mapper.resolve_string(v).await?);
        }
        builder = builder.with_allowed_repositories(dto.allowed_repositories.clone());
        builder = builder.with_allowed_profiles(dto.allowed_profiles.clone());
        builder = builder.with_allowed_models(dto.allowed_models.clone());
        builder = builder.with_trusted_assignment_author_database_id(
            mapper
                .resolve_typed(&dto.trusted_assignment_author_database_id)
                .await?,
        );
        if let Some(actor_type) = dto.trusted_assignment_author_type {
            builder = builder.with_trusted_assignment_author_type(actor_type);
        }
        builder = builder.with_trusted_execution_author_database_id(
            mapper
                .resolve_typed(&dto.trusted_execution_author_database_id)
                .await?,
        );
        if let Some(actor_type) = dto.trusted_execution_author_type {
            builder = builder.with_trusted_execution_author_type(actor_type);
        }
        builder = builder.with_expected_project_status_field_node_id(
            mapper
                .resolve_string(&dto.expected_project_status_field_node_id)
                .await?,
        );
        if let Some(ref v) = dto.request_timeout_ms {
            builder = builder.with_request_timeout_ms(mapper.resolve_typed(v).await?);
        }
        if let Some(ref c) = dto.comment_api {
            builder = builder.with_comment_api(map_comment_api(c, &mapper).await?);
        }
        if let Some(ref v) = dto.strict_recovery {
            builder = builder.with_strict_recovery(mapper.resolve_typed(v).await?);
        }
        if let Some(ref cap) = dto.priority_queue_capacity {
            let resolved: u64 = mapper.resolve_typed(cap).await?;
            builder = builder.with_priority_queue_capacity(resolved as usize);
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_reaction_resolves_token_from_env() {
        std::env::set_var("COPILOT_AGENT_TASK_TEST_TOKEN", "ghp_from_env");
        let cfg = serde_json::json!({
            "token": "${COPILOT_AGENT_TASK_TEST_TOKEN}",
            "allowedRepositories": ["drasi-project/drasi-core"],
            "allowedProfiles": ["issue-validator"],
            "allowedModels": ["gpt-5"],
            "trustedAssignmentAuthorDatabaseId": 4021243,
            "trustedAssignmentAuthorType": "Bot",
            "trustedExecutionAuthorDatabaseId": 90210,
            "trustedExecutionAuthorType": "Bot",
            "expectedProjectStatusFieldNodeId": "PVTSSF_status",
        });
        let reaction = CopilotAgentTaskReactionDescriptor
            .create_reaction("id", vec!["q1".to_string()], &cfg, true)
            .await
            .expect("create_reaction succeeds");
        assert_eq!(reaction.type_name(), "copilot-agent-task");
        std::env::remove_var("COPILOT_AGENT_TASK_TEST_TOKEN");
    }

    #[test]
    fn generated_schema_exposes_numeric_token_owner_guard() {
        let schema = CopilotAgentTaskReactionDescriptor.config_schema_json();
        let schemas: serde_json::Value =
            serde_json::from_str(&schema).expect("generated schema is JSON");
        assert!(
            schemas["reaction.copilot_agent_task.CopilotAgentTaskReactionConfig"]["properties"]
                ["expectedGithubUserId"]
                .is_object()
        );
        for referenced_schema in [
            "ConfigValueString",
            "ConfigValueU64",
            "ConfigValueBool",
            "workgraph.ActorType",
        ] {
            assert!(
                schemas[referenced_schema].is_object(),
                "schema reference {referenced_schema} must resolve"
            );
        }
    }

    #[tokio::test]
    async fn create_reaction_rejects_empty_allowlists() {
        let cfg = serde_json::json!({
            "token": "ghp_test",
            "allowedRepositories": [],
            "allowedProfiles": [],
            "allowedModels": [],
            "trustedAssignmentAuthorDatabaseId": 4021243,
            "trustedAssignmentAuthorType": "Bot",
            "trustedExecutionAuthorDatabaseId": 90210,
            "trustedExecutionAuthorType": "Bot",
            "expectedProjectStatusFieldNodeId": "PVTSSF_status",
        });
        let result = CopilotAgentTaskReactionDescriptor
            .create_reaction("id", vec!["q1".to_string()], &cfg, true)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_reaction_rejects_non_strict_recovery() {
        let cfg = serde_json::json!({
            "token": "ghp_test",
            "allowedRepositories": ["drasi-project/drasi-core"],
            "allowedProfiles": ["issue-validator"],
            "allowedModels": ["gpt-5"],
            "trustedAssignmentAuthorDatabaseId": 4021243,
            "trustedAssignmentAuthorType": "Bot",
            "trustedExecutionAuthorDatabaseId": 90210,
            "trustedExecutionAuthorType": "Bot",
            "expectedProjectStatusFieldNodeId": "PVTSSF_status",
            "strictRecovery": false,
        });
        let result = CopilotAgentTaskReactionDescriptor
            .create_reaction("id", vec!["q1".to_string()], &cfg, true)
            .await;
        assert!(result.is_err());
    }
}
