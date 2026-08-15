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

//! Descriptor for the WorkGraph admission reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use drasi_workgraph_common::trust::ActorType;

use crate::config::WorkgraphAdmissionReactionConfig;
use crate::WorkgraphAdmissionReactionBuilder;

/// Declarative configuration DTO for the admission reaction.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::workgraph_admission::WorkgraphAdmissionReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkgraphAdmissionReactionConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_rest_url: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_graphql_url: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub github_token_env: Option<ConfigValue<String>>,

    pub allowed_repositories: Vec<String>,
    pub allowed_projects: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub project_status_field_name: Option<ConfigValue<String>>,

    #[schema(value_type = ConfigValueString)]
    pub expected_project_status_field_node_id: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub expected_source_status: ConfigValue<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub agent_profile: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub profile_base_ref: Option<ConfigValue<String>>,

    /// The numeric GitHub database ID of the identity this reaction posts as.
    /// Together with `trustedAuthorType` this is the whole trust key: no node
    /// ID and no GitHub App ID is configured or accepted.
    #[schema(value_type = ConfigValueU64)]
    pub trusted_author_database_id: ConfigValue<u64>,

    /// The actor type of the identity this reaction posts as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_author_type: Option<ActorType>,

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

impl From<&WorkgraphAdmissionReactionConfig> for WorkgraphAdmissionReactionConfigDto {
    fn from(config: &WorkgraphAdmissionReactionConfig) -> Self {
        Self {
            github_rest_url: Some(ConfigValue::Static(config.github_rest_url.clone())),
            github_graphql_url: Some(ConfigValue::Static(config.github_graphql_url.clone())),
            github_token_env: Some(ConfigValue::Static(config.github_token_env.clone())),
            allowed_repositories: config.allowed_repositories.clone(),
            allowed_projects: config.allowed_projects.clone(),
            project_status_field_name: Some(ConfigValue::Static(
                config.project_status_field_name.clone(),
            )),
            expected_project_status_field_node_id: ConfigValue::Static(
                config.expected_project_status_field_node_id.clone(),
            ),
            expected_source_status: ConfigValue::Static(config.expected_source_status.clone()),
            agent_profile: Some(ConfigValue::Static(config.agent_profile.clone())),
            profile_base_ref: Some(ConfigValue::Static(config.profile_base_ref.clone())),
            trusted_author_database_id: ConfigValue::Static(config.trusted_author_database_id),
            trusted_author_type: Some(config.trusted_author_type),
            timeout_secs: Some(ConfigValue::Static(config.timeout_secs)),
            strict_recovery: Some(ConfigValue::Static(config.strict_recovery)),
            priority_queue_capacity: None,
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    WorkgraphAdmissionReactionConfigDto,
    ActorType,
    ConfigValueStringSchema,
    ConfigValueU64Schema,
    ConfigValueBoolSchema,
)))]
struct WorkgraphAdmissionReactionSchemas;

/// Descriptor for the WorkGraph admission reaction plugin.
pub struct WorkgraphAdmissionReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for WorkgraphAdmissionReactionDescriptor {
    fn kind(&self) -> &str {
        "workgraph-admission"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.workgraph_admission.WorkgraphAdmissionReactionConfig"
    }

    fn display_name(&self) -> &str {
        "WorkGraph Admission"
    }

    fn display_description(&self) -> &str {
        "Assigns the issue-validation responsibility as one WorkGraphEvent/v1 comment and admits the Project Item to AwaitingValidation, with durable intent-before-side-effect recovery."
    }

    fn display_icon(&self) -> &str {
        "github"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = WorkgraphAdmissionReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize config schema");

        SchemaUiAnnotator::new(
            schemas,
            "reaction.workgraph_admission.WorkgraphAdmissionReactionConfig",
        )
        .expect("root schema not found")
        .field("githubRestUrl", |f| {
            f.group("GitHub")
                .order(1)
                .placeholder("https://api.github.com")
        })
        .field("githubGraphqlUrl", |f| {
            f.group("GitHub")
                .order(2)
                .placeholder("https://api.github.com/graphql")
        })
        .field("githubTokenEnv", |f| {
            f.group("GitHub").order(3).placeholder("GITHUB_TOKEN")
        })
        .field("allowedRepositories", |f| f.group("Allowlists").order(10))
        .field("allowedProjects", |f| f.group("Allowlists").order(11))
        .field("trustedAuthorDatabaseId", |f| {
            f.group("Allowlists").order(12).placeholder("4021243")
        })
        .field("trustedAuthorType", |f| f.group("Allowlists").order(13))
        .field("projectStatusFieldName", |f| f.group("Project").order(20))
        .field("expectedProjectStatusFieldNodeId", |f| {
            f.group("Project").order(21).placeholder("PVTSSF_...")
        })
        .field("expectedSourceStatus", |f| f.group("Project").order(22))
        .field("agentProfile", |f| {
            f.group("Responsibility")
                .order(30)
                .placeholder("issue-validator")
        })
        .field("profileBaseRef", |f| {
            f.group("Responsibility").order(31).placeholder("main")
        })
        .field("timeoutSecs", |f| f.group("Advanced").order(40))
        .field("strictRecovery", |f| f.group("Advanced").order(41))
        .field("priorityQueueCapacity", |f| {
            f.group("Advanced").order(42).placeholder("10000")
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
        let dto: WorkgraphAdmissionReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = WorkgraphAdmissionReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_allowed_repositories(dto.allowed_repositories.clone())
            .with_allowed_projects(dto.allowed_projects.clone())
            .with_trusted_author_database_id(
                mapper
                    .resolve_typed(&dto.trusted_author_database_id)
                    .await?,
            )
            .with_expected_project_status_field_node_id(
                mapper
                    .resolve_string(&dto.expected_project_status_field_node_id)
                    .await?,
            )
            .with_expected_source_status(mapper.resolve_string(&dto.expected_source_status).await?);

        if let Some(actor_type) = dto.trusted_author_type {
            builder = builder.with_trusted_author_type(actor_type);
        }

        if let Some(ref value) = dto.github_rest_url {
            builder = builder.with_github_rest_url(mapper.resolve_string(value).await?);
        }
        if let Some(ref value) = dto.github_graphql_url {
            builder = builder.with_github_graphql_url(mapper.resolve_string(value).await?);
        }
        if let Some(ref value) = dto.github_token_env {
            builder = builder.with_github_token_env(mapper.resolve_string(value).await?);
        }
        if let Some(ref value) = dto.project_status_field_name {
            builder = builder.with_project_status_field_name(mapper.resolve_string(value).await?);
        }
        if let Some(ref value) = dto.agent_profile {
            builder = builder.with_agent_profile(mapper.resolve_string(value).await?);
        }
        if let Some(ref value) = dto.profile_base_ref {
            builder = builder.with_profile_base_ref(mapper.resolve_string(value).await?);
        }
        if let Some(ref value) = dto.timeout_secs {
            builder = builder.with_timeout_secs(mapper.resolve_typed(value).await?);
        }
        if let Some(ref value) = dto.strict_recovery {
            builder = builder.with_strict_recovery(mapper.resolve_typed(value).await?);
        }
        if let Some(ref value) = dto.priority_queue_capacity {
            let resolved: u64 = mapper.resolve_typed(value).await?;
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

    fn config_json() -> serde_json::Value {
        serde_json::json!({
            "allowedRepositories": ["drasi-project/drasi-core"],
            "allowedProjects": ["PVT_project"],
            "expectedProjectStatusFieldNodeId": "PVTSSF_status",
            "expectedSourceStatus": "Triage",
            "trustedAuthorDatabaseId": 4021243,
            "trustedAuthorType": "Bot"
        })
    }

    #[tokio::test]
    async fn create_reaction_builds_from_declarative_config() {
        let reaction = WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config_json(), true)
            .await
            .expect("descriptor creates the reaction");
        assert_eq!(reaction.type_name(), "workgraph-admission");
        assert!(reaction.is_durable());
    }

    #[tokio::test]
    async fn create_reaction_resolves_env_references() {
        std::env::set_var("WORKGRAPH_ADMISSION_TEST_STATUS", "Triage");
        let mut config = config_json();
        config["expectedSourceStatus"] = serde_json::json!("${WORKGRAPH_ADMISSION_TEST_STATUS}");
        WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config, true)
            .await
            .expect("env reference resolves");
        std::env::remove_var("WORKGRAPH_ADMISSION_TEST_STATUS");
    }

    #[tokio::test]
    async fn create_reaction_rejects_empty_allowlists() {
        let mut config = config_json();
        config["allowedRepositories"] = serde_json::json!([]);
        assert!(WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config, true)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn create_reaction_rejects_unknown_fields() {
        let mut config = config_json();
        config["arbitraryEndpoint"] = serde_json::json!("https://evil.example");
        assert!(WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config, true)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn create_reaction_requires_the_two_trust_values_and_nothing_else() {
        // A zero (or missing) database ID cannot identify an account.
        let mut config = config_json();
        config["trustedAuthorDatabaseId"] = serde_json::json!(0);
        let error = match WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config, true)
            .await
        {
            Ok(_) => panic!("a zero database ID must be rejected"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("trustedAuthorDatabaseId"),
            "{error}"
        );

        // The actor type is optional in config and defaults to `Bot`.
        let mut config = config_json();
        config
            .as_object_mut()
            .expect("object")
            .remove("trustedAuthorType");
        WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config, true)
            .await
            .expect("actorType defaults to Bot");

        // Node IDs are audit data, not part of the configured trust contract.
        for removed in ["trustedAuthorNodeId", "trustedAuthors"] {
            let mut config = config_json();
            config[removed] = serde_json::json!("x");
            assert!(
                WorkgraphAdmissionReactionDescriptor
                    .create_reaction("admission", vec!["admit".to_string()], &config, true)
                    .await
                    .is_err(),
                "removed trust field '{removed}' must be rejected"
            );
        }
    }

    #[tokio::test]
    async fn create_reaction_rejects_plaintext_endpoints() {
        let mut config = config_json();
        config["githubRestUrl"] = serde_json::json!("http://api.example.com");
        assert!(WorkgraphAdmissionReactionDescriptor
            .create_reaction("admission", vec!["admit".to_string()], &config, true)
            .await
            .is_err());
    }

    #[test]
    fn generated_schema_resolves_referenced_config_values() {
        let schema = WorkgraphAdmissionReactionDescriptor.config_schema_json();
        let schemas: serde_json::Value =
            serde_json::from_str(&schema).expect("generated schema is JSON");
        assert!(
            schemas["reaction.workgraph_admission.WorkgraphAdmissionReactionConfig"]["properties"]
                ["expectedSourceStatus"]
                .is_object()
        );
        for referenced in [
            "ConfigValueString",
            "ConfigValueU64",
            "ConfigValueBool",
            "workgraph.ActorType",
        ] {
            assert!(
                schemas[referenced].is_object(),
                "schema reference {referenced} must resolve"
            );
        }
    }
}
