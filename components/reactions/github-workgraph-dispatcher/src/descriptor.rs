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

use crate::config::GitHubWorkGraphDispatcherConfig;
use crate::GitHubWorkGraphDispatcherBuilder;
use drasi_lib::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::github_workgraph_dispatcher::Config)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DispatcherConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub api_url: Option<ConfigValue<String>>,
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub user_agent: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub api_version: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schema(value_type = Object)]
    pub headers: HashMap<String, ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub request_timeout_ms: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub max_attempts: Option<ConfigValue<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub initial_retry_delay_ms: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    DispatcherConfigDto,
    ConfigValueStringSchema,
    ConfigValueU32Schema,
    ConfigValueU64Schema,
)))]
struct DispatcherSchemas;

pub struct GitHubWorkGraphDispatcherDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for GitHubWorkGraphDispatcherDescriptor {
    fn kind(&self) -> &str {
        "github-workgraph-dispatcher"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.github_workgraph_dispatcher.Config"
    }

    fn display_name(&self) -> &str {
        "GitHub WorkGraph Dispatcher"
    }

    fn display_description(&self) -> &str {
        "Durably leases queued GitHub WorkGraph tasks to available worker slots."
    }

    fn display_icon(&self) -> &str {
        "github"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = DispatcherSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");
        SchemaUiAnnotator::new(schemas, "reaction.github_workgraph_dispatcher.Config")
            .expect("root schema not found")
            .field("apiUrl", |field| field.group("GitHub").order(1))
            .field("token", |field| {
                field.group("GitHub").order(2).widget("password")
            })
            .field("userAgent", |field| field.group("GitHub").order(3))
            .field("apiVersion", |field| field.group("GitHub").order(4))
            .field("headers", |field| field.group("GitHub").order(5))
            .field("requestTimeoutMs", |field| {
                field.group("Retry and Recovery").order(10)
            })
            .field("maxAttempts", |field| {
                field.group("Retry and Recovery").order(11)
            })
            .field("initialRetryDelayMs", |field| {
                field.group("Retry and Recovery").order(12)
            })
            .field("priorityQueueCapacity", |field| {
                field.group("Advanced").order(20)
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
        let dto: DispatcherConfigDto = serde_json::from_value(config_json.clone())?;
        match &dto.token {
            ConfigValue::Secret { name } if !name.trim().is_empty() => {}
            _ => anyhow::bail!("token must be a non-empty Secret reference"),
        }
        let mapper = DtoMapper::new();
        let mut config = GitHubWorkGraphDispatcherConfig {
            token: mapper.resolve_string(&dto.token).await?,
            ..Default::default()
        };
        if let Some(value) = &dto.api_url {
            config.api_url = mapper.resolve_string(value).await?;
        }
        if let Some(value) = &dto.user_agent {
            config.user_agent = mapper.resolve_string(value).await?;
        }
        if let Some(value) = &dto.api_version {
            config.api_version = mapper.resolve_string(value).await?;
        }
        for (name, value) in &dto.headers {
            config
                .headers
                .insert(name.clone(), mapper.resolve_string(value).await?);
        }
        if let Some(value) = &dto.request_timeout_ms {
            config.request_timeout_ms = mapper.resolve_typed(value).await?;
        }
        if let Some(value) = &dto.max_attempts {
            config.max_attempts = mapper.resolve_typed(value).await?;
        }
        if let Some(value) = &dto.initial_retry_delay_ms {
            config.initial_retry_delay_ms = mapper.resolve_typed(value).await?;
        }
        if let Some(value) = &dto.priority_queue_capacity {
            let capacity: u64 = mapper.resolve_typed(value).await?;
            config.priority_queue_capacity = usize::try_from(capacity)
                .map_err(|_| anyhow::anyhow!("priorityQueueCapacity exceeds usize"))?;
        }
        config.validate(&query_ids)?;
        let mut reaction = GitHubWorkGraphDispatcherBuilder::new(id)
            .with_queries(query_ids)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        reaction.base.set_raw_config(config_json.clone());
        Ok(Box::new(reaction))
    }
}
