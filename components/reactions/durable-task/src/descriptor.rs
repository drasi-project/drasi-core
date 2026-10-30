// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Declarative configuration and dynamic plugin discovery.

use std::collections::HashMap;

use drasi_lib::{recovery::ReactionRecoveryPolicy, Reaction};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{
    config::{default_retries, default_retry_delay},
    Action, Authentication, DurableTaskQueryConfig, DurableTaskReaction, DurableTaskReactionConfig,
    DurableTaskTemplate, TargetPolicy,
};

/// Recovery policies supported without automatically resetting command identity.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::durable_task::RecoveryPolicy)]
#[serde(rename_all = "camelCase")]
pub enum RecoveryPolicyDto {
    Strict,
    AutoSkipGap,
}

impl From<RecoveryPolicyDto> for ReactionRecoveryPolicy {
    fn from(value: RecoveryPolicyDto) -> Self {
        match value {
            RecoveryPolicyDto::Strict => Self::Strict,
            RecoveryPolicyDto::AutoSkipGap => Self::AutoSkipGap,
        }
    }
}

impl From<ReactionRecoveryPolicy> for RecoveryPolicyDto {
    fn from(value: ReactionRecoveryPolicy) -> Self {
        match value {
            ReactionRecoveryPolicy::Strict => Self::Strict,
            ReactionRecoveryPolicy::AutoSkipGap => Self::AutoSkipGap,
            ReactionRecoveryPolicy::AutoReset => unreachable!("builder rejects AutoReset"),
        }
    }
}

/// JSON payload template plus a typed dispatch action.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::durable_task::Template)]
pub struct TemplateDto {
    #[serde(default)]
    pub template: String,
    #[serde(flatten)]
    pub action: Action,
}

/// Actions configured independently for each result operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::durable_task::QueryConfig)]
#[serde(deny_unknown_fields)]
pub struct QueryConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateDto>,
}

impl From<QueryConfigDto> for DurableTaskQueryConfig {
    fn from(value: QueryConfigDto) -> Self {
        fn spec(dto: TemplateDto) -> DurableTaskTemplate {
            DurableTaskTemplate::with_extension(dto.template, dto.action)
        }
        Self {
            added: value.added.map(spec),
            updated: value.updated.map(spec),
            deleted: value.deleted.map(spec),
        }
    }
}

impl From<&DurableTaskQueryConfig> for QueryConfigDto {
    fn from(value: &DurableTaskQueryConfig) -> Self {
        fn spec(value: &DurableTaskTemplate) -> TemplateDto {
            TemplateDto {
                template: value.template.clone(),
                action: value.extension.clone(),
            }
        }
        Self {
            added: value.added.as_ref().map(spec),
            updated: value.updated.as_ref().map(spec),
            deleted: value.deleted.as_ref().map(spec),
        }
    }
}

/// Configuration accepted by the `durable-task` plugin descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::durable_task::DurableTaskReactionConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DurableTaskReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub task_hub: ConfigValue<String>,
    #[schema(value_type = ConfigValueString)]
    pub producer_id: ConfigValue<String>,
    #[serde(default)]
    pub authentication: Authentication,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub scope: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfigDto>,
    #[serde(default)]
    pub routes: HashMap<String, QueryConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<RecoveryPolicyDto>,
}

impl From<&DurableTaskReactionConfig> for DurableTaskReactionConfigDto {
    fn from(value: &DurableTaskReactionConfig) -> Self {
        Self {
            endpoint: ConfigValue::Static(value.endpoint.clone()),
            task_hub: ConfigValue::Static(value.task_hub.clone()),
            producer_id: ConfigValue::Static(value.producer_id.clone()),
            authentication: value.authentication,
            scope: Some(ConfigValue::Static(value.scope.clone())),
            timeout_ms: Some(ConfigValue::Static(value.timeout_ms)),
            max_retries: value.max_retries,
            retry_delay_ms: value.retry_delay_ms,
            default_template: value.default_template.as_ref().map(Into::into),
            routes: value
                .routes
                .iter()
                .map(|(key, route)| (key.clone(), route.into()))
                .collect(),
            priority_queue_capacity: None,
            recovery_policy: None,
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    DurableTaskReactionConfigDto,
    QueryConfigDto,
    TemplateDto,
    Action,
    Authentication,
    TargetPolicy,
    RecoveryPolicyDto,
)))]
struct Schemas;

/// Descriptor used by hosts to discover and construct the reaction.
pub struct DurableTaskReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for DurableTaskReactionDescriptor {
    fn kind(&self) -> &str {
        "durable-task"
    }
    fn config_version(&self) -> &str {
        "1.0.0"
    }
    fn config_schema_name(&self) -> &str {
        "reaction.durable_task.DurableTaskReactionConfig"
    }
    fn display_name(&self) -> &str {
        "Durable Task Scheduler"
    }
    fn display_description(&self) -> &str {
        "Starts, signals or terminates Durable Task workflows using configurable query-result actions."
    }
    fn display_icon(&self) -> &str {
        "workflow"
    }
    fn config_schema_json(&self) -> String {
        let schemas = Schemas::openapi()
            .components
            .expect("derived schema has components")
            .schemas;
        serde_json::to_string(&schemas).expect("derived schema is serializable")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: DurableTaskReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();
        let mut config = DurableTaskReactionConfig {
            endpoint: mapper.resolve_string(&dto.endpoint).await?,
            task_hub: mapper.resolve_string(&dto.task_hub).await?,
            producer_id: mapper.resolve_string(&dto.producer_id).await?,
            authentication: dto.authentication,
            max_retries: dto.max_retries,
            retry_delay_ms: dto.retry_delay_ms,
            default_template: dto.default_template.map(Into::into),
            routes: dto
                .routes
                .into_iter()
                .map(|(key, route)| (key, route.into()))
                .collect(),
            ..Default::default()
        };
        if let Some(scope) = dto.scope {
            config.scope = mapper.resolve_string(&scope).await?;
        }
        if let Some(timeout) = dto.timeout_ms {
            config.timeout_ms = mapper.resolve_typed(&timeout).await?;
        }
        let mut builder = DurableTaskReaction::builder(id)
            .with_queries(query_ids)
            .with_config(config)
            .with_auto_start(auto_start);
        if let Some(capacity) = dto.priority_queue_capacity {
            let value: u64 = mapper.resolve_typed(&capacity).await?;
            builder = builder.with_priority_queue_capacity(usize::try_from(value)?);
        }
        if let Some(policy) = dto.recovery_policy {
            builder = builder.with_recovery_policy(policy.into());
        }
        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());
        Ok(Box::new(reaction))
    }
}
