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

//! Descriptor for the GitHub project-item refresh reaction plugin.

use std::collections::HashMap;

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::GitHubProjectItemRefreshConfig;
use crate::GitHubProjectItemRefreshBuilder;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::github_project_item_refresh::GitHubProjectItemRefreshConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GitHubProjectItemRefreshConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub github_token: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub graphql_url: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub graphql_headers: HashMap<String, ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowlisted_project_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub status_field_name: Option<ConfigValue<String>>,
    #[schema(value_type = ConfigValueString)]
    pub destination_event_url: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub destination_bearer_secret: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub request_timeout_ms: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub delivery_record_ttl_secs: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,
    /// Recovery policy for publication failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<RecoveryPolicyDto>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::github_project_item_refresh::RecoveryPolicy)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicyDto {
    Strict,
    AutoSkipGap,
}

impl From<RecoveryPolicyDto> for drasi_lib::recovery::ReactionRecoveryPolicy {
    fn from(value: RecoveryPolicyDto) -> Self {
        match value {
            RecoveryPolicyDto::Strict => Self::Strict,
            RecoveryPolicyDto::AutoSkipGap => Self::AutoSkipGap,
        }
    }
}

impl From<&GitHubProjectItemRefreshConfig> for GitHubProjectItemRefreshConfigDto {
    fn from(config: &GitHubProjectItemRefreshConfig) -> Self {
        let graphql_headers = config
            .graphql_headers
            .iter()
            .map(|(key, value)| (key.clone(), ConfigValue::Static(value.clone())))
            .collect::<HashMap<_, _>>();

        Self {
            github_token: ConfigValue::Static(config.github_token.clone()),
            graphql_url: Some(ConfigValue::Static(config.graphql_url.clone())),
            graphql_headers,
            allowlisted_project_ids: config.allowlisted_project_ids.clone(),
            status_field_name: Some(ConfigValue::Static(config.status_field_name.clone())),
            destination_event_url: ConfigValue::Static(config.destination_event_url.clone()),
            destination_bearer_secret: config
                .destination_bearer_secret
                .as_ref()
                .map(|secret| ConfigValue::Static(secret.clone())),
            request_timeout_ms: Some(ConfigValue::Static(config.request_timeout_ms)),
            delivery_record_ttl_secs: Some(ConfigValue::Static(config.delivery_record_ttl_secs)),
            priority_queue_capacity: None,
            recovery_policy: None,
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(GitHubProjectItemRefreshConfigDto, RecoveryPolicyDto,)))]
struct GitHubProjectItemRefreshSchemas;

/// Descriptor for the GitHub project-item refresh reaction plugin.
pub struct GitHubProjectItemRefreshDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for GitHubProjectItemRefreshDescriptor {
    fn kind(&self) -> &str {
        "github-project-item-refresh"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.github_project_item_refresh.GitHubProjectItemRefreshConfig"
    }

    fn display_name(&self) -> &str {
        "GitHub Project Item Refresh"
    }

    fn display_description(&self) -> &str {
        "Hydrates authoritative GitHub ProjectV2 item status and republishes it to a standard HTTP source."
    }

    fn display_icon(&self) -> &str {
        "github"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = GitHubProjectItemRefreshSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");

        SchemaUiAnnotator::new(
            schemas,
            "reaction.github_project_item_refresh.GitHubProjectItemRefreshConfig",
        )
        .expect("root schema not found")
        .field("githubToken", |f| {
            f.group("GitHub").order(1).widget("password")
        })
        .field("graphqlUrl", |f| {
            f.group("GitHub")
                .order(2)
                .placeholder("https://api.github.com/graphql")
        })
        .field("graphqlHeaders", |f| f.group("GitHub").order(3))
        .field("allowlistedProjectIds", |f| f.group("GitHub").order(4))
        .field("statusFieldName", |f| {
            f.group("GitHub").order(5).placeholder("Status")
        })
        .field("destinationEventUrl", |f| f.group("Destination").order(10))
        .field("destinationBearerSecret", |f| {
            f.group("Destination").order(11).widget("password")
        })
        .field("requestTimeoutMs", |f| {
            f.group("Advanced").order(20).placeholder("10000")
        })
        .field("deliveryRecordTtlSecs", |f| {
            f.group("Advanced").order(21).placeholder("604800")
        })
        .field("priorityQueueCapacity", |f| {
            f.group("Advanced").order(22).placeholder("10000")
        })
        .field("recoveryPolicy", |f| f.group("Advanced").order(23))
        .annotate()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: GitHubProjectItemRefreshConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = GitHubProjectItemRefreshBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_github_token(mapper.resolve_string(&dto.github_token).await?)
            .with_destination_event_url(mapper.resolve_string(&dto.destination_event_url).await?);

        if let Some(graphql_url) = &dto.graphql_url {
            builder = builder.with_graphql_url(mapper.resolve_string(graphql_url).await?);
        }
        if !dto.graphql_headers.is_empty() {
            let mut resolved_headers = HashMap::new();
            for (name, value) in &dto.graphql_headers {
                resolved_headers.insert(name.clone(), mapper.resolve_string(value).await?);
            }
            for (name, value) in resolved_headers {
                builder = builder.with_graphql_header(name, value);
            }
        }
        if !dto.allowlisted_project_ids.is_empty() {
            builder = builder.with_allowlisted_project_ids(dto.allowlisted_project_ids.clone());
        }
        if let Some(status_field_name) = &dto.status_field_name {
            builder =
                builder.with_status_field_name(mapper.resolve_string(status_field_name).await?);
        }
        if let Some(secret) = &dto.destination_bearer_secret {
            builder = builder.with_destination_bearer_secret(mapper.resolve_string(secret).await?);
        }
        if let Some(timeout_ms) = &dto.request_timeout_ms {
            builder = builder.with_request_timeout_ms(mapper.resolve_typed(timeout_ms).await?);
        }
        if let Some(ttl_secs) = &dto.delivery_record_ttl_secs {
            builder = builder.with_delivery_record_ttl_secs(mapper.resolve_typed(ttl_secs).await?);
        }
        if let Some(cap) = &dto.priority_queue_capacity {
            let resolved: u64 = mapper.resolve_typed(cap).await?;
            builder = builder.with_priority_queue_capacity(resolved as usize);
        }
        if let Some(policy) = dto.recovery_policy {
            builder = builder.with_recovery_policy(policy.into());
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());
        Ok(Box::new(reaction))
    }
}
