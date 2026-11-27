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

//! Plugin descriptor and configuration DTO for the AHP reaction.

use std::collections::HashMap;

use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{AhpMode, AhpReactionBuilder, AhpReactionConfig, FollowUpKind, OnDelete};

/// Session or automation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::Mode)]
#[serde(rename_all = "snake_case")]
pub enum AhpModeDto {
    Session,
    Automation,
}

/// Pending-message kind for follow-ups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::FollowUpKind)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpKindDto {
    Queued,
    Steering,
}

/// Behavior on `DELETE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::OnDelete)]
#[serde(rename_all = "snake_case")]
pub enum OnDeleteDto {
    None,
    Cancel,
    Archive,
    Dispose,
}

/// Recovery policy when the checkpoint gap was pruned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::RecoveryPolicy)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicyDto {
    Strict,
    AutoSkipGap,
}

/// Handlebars message template.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::TemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateSpecDto {
    /// Message text template.
    #[serde(default)]
    pub template: String,
}

/// Templates per operation: `added` = initial message, `updated` =
/// follow-up, `deleted` = closing message.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::QueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QueryConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpecDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpecDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpecDto>,
}

/// Plugin configuration for the AHP reaction.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::ahp::AhpReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AhpReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub token: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub auth_resource: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub client_id: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub session_ready_timeout_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AhpModeDto>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_key_fields: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub provider: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working_directories: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub session_config: Option<serde_json::Map<String, serde_json::Value>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_template: Option<String>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfigDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfigDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_kind: Option<FollowUpKindDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<OnDeleteDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub automation: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub durable: Option<ConfigValue<bool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<RecoveryPolicyDto>,
}

fn template_to_dto(spec: &TemplateSpec) -> TemplateSpecDto {
    TemplateSpecDto {
        template: spec.template.clone(),
    }
}

fn query_config_to_dto(config: &QueryConfig) -> QueryConfigDto {
    QueryConfigDto {
        added: config.added.as_ref().map(template_to_dto),
        updated: config.updated.as_ref().map(template_to_dto),
        deleted: config.deleted.as_ref().map(template_to_dto),
    }
}

fn query_config_from_dto(dto: &QueryConfigDto) -> QueryConfig {
    let spec = |t: &Option<TemplateSpecDto>| t.as_ref().map(|t| TemplateSpec::new(&t.template));
    QueryConfig {
        added: spec(&dto.added),
        updated: spec(&dto.updated),
        deleted: spec(&dto.deleted),
    }
}

impl From<&AhpReactionConfig> for AhpReactionConfigDto {
    fn from(config: &AhpReactionConfig) -> Self {
        Self {
            endpoint: ConfigValue::Static(config.endpoint.clone()),
            token: config.token.clone().map(ConfigValue::Static),
            auth_resource: config.auth_resource.clone().map(ConfigValue::Static),
            client_id: config.client_id.clone().map(ConfigValue::Static),
            timeout_ms: Some(ConfigValue::Static(config.timeout_ms)),
            session_ready_timeout_ms: Some(ConfigValue::Static(config.session_ready_timeout_ms)),
            mode: Some(match config.mode {
                AhpMode::Session => AhpModeDto::Session,
                AhpMode::Automation => AhpModeDto::Automation,
            }),
            result_key_fields: config.result_key_fields.clone(),
            provider: config.provider.clone().map(ConfigValue::Static),
            working_directories: config.working_directories.clone(),
            session_config: config.session_config.clone(),
            title_template: config.title_template.clone(),
            routes: config
                .routes
                .iter()
                .map(|(k, v)| (k.clone(), query_config_to_dto(v)))
                .collect(),
            default_template: config.default_template.as_ref().map(query_config_to_dto),
            follow_up_kind: Some(match config.follow_up_kind {
                FollowUpKind::Queued => FollowUpKindDto::Queued,
                FollowUpKind::Steering => FollowUpKindDto::Steering,
            }),
            on_delete: Some(match config.on_delete {
                OnDelete::None => OnDeleteDto::None,
                OnDelete::Cancel => OnDeleteDto::Cancel,
                OnDelete::Archive => OnDeleteDto::Archive,
                OnDelete::Dispose => OnDeleteDto::Dispose,
            }),
            automation: config.automation.clone().map(ConfigValue::Static),
            durable: Some(ConfigValue::Static(config.durable)),
            priority_queue_capacity: None,
            recovery_policy: None,
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    AhpReactionConfigDto,
    AhpModeDto,
    FollowUpKindDto,
    OnDeleteDto,
    RecoveryPolicyDto,
    TemplateSpecDto,
    QueryConfigDto
)))]
struct AhpReactionSchemas;

/// Plugin descriptor for the `ahp` reaction kind.
pub struct AhpReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for AhpReactionDescriptor {
    fn kind(&self) -> &str {
        "ahp"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.ahp.AhpReactionConfig"
    }

    fn display_name(&self) -> &str {
        "Agent Host Protocol"
    }

    fn display_description(&self) -> &str {
        "Starts and steers agent sessions or automations on an AHP host from query changes."
    }

    fn display_icon(&self) -> &str {
        "bot"
    }

    fn config_schema_json(&self) -> String {
        let api = AhpReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize config schema");

        drasi_plugin_sdk::schema_ui::SchemaUiAnnotator::new(
            schemas,
            "reaction.ahp.AhpReactionConfig",
        )
        .expect("root schema not found")
        .field("endpoint", |f| {
            f.group("Connection")
                .order(1)
                .placeholder("wss://agent-host.example.com/ahp")
        })
        .field("token", |f| {
            f.group("Connection").order(2).widget("password")
        })
        .field("authResource", |f| f.group("Connection").order(3))
        .field("clientId", |f| f.group("Connection").order(4))
        .field("timeoutMs", |f| {
            f.group("Connection").order(5).placeholder("10000")
        })
        .field("sessionReadyTimeoutMs", |f| {
            f.group("Connection").order(6).placeholder("60000")
        })
        .field("mode", |f| f.group("Behavior").order(10))
        .field("resultKeyFields", |f| f.group("Behavior").order(11))
        .field("automation", |f| f.group("Behavior").order(12))
        .field("followUpKind", |f| f.group("Behavior").order(13))
        .field("onDelete", |f| f.group("Behavior").order(14))
        .field("provider", |f| f.group("Session").order(20))
        .field("workingDirectories", |f| f.group("Session").order(21))
        .field("sessionConfig", |f| f.group("Session").order(22))
        .field("titleTemplate", |f| f.group("Messages").order(30))
        .field("defaultTemplate", |f| f.group("Messages").order(31))
        .field("routes", |f| f.group("Messages").order(32))
        .field("durable", |f| f.group("Advanced").order(40))
        .field("priorityQueueCapacity", |f| {
            f.group("Advanced").order(41).placeholder("10000")
        })
        .field("recoveryPolicy", |f| f.group("Advanced").order(42))
        .annotate()
        .to_string()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: AhpReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = AhpReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint).await?)
            .with_result_key_fields(dto.result_key_fields.clone())
            .with_working_directories(dto.working_directories.clone());

        if let Some(v) = &dto.token {
            builder = builder.with_token(mapper.resolve_string(v).await?);
        }
        if let Some(v) = &dto.auth_resource {
            builder = builder.with_auth_resource(mapper.resolve_string(v).await?);
        }
        if let Some(v) = &dto.client_id {
            builder = builder.with_client_id(mapper.resolve_string(v).await?);
        }
        if let Some(v) = &dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(v).await?);
        }
        if let Some(v) = &dto.session_ready_timeout_ms {
            builder = builder.with_session_ready_timeout_ms(mapper.resolve_typed(v).await?);
        }
        if let Some(v) = &dto.automation {
            builder = builder.with_automation(mapper.resolve_string(v).await?);
        }
        let mode = dto.mode.unwrap_or(if dto.automation.is_some() {
            AhpModeDto::Automation
        } else {
            AhpModeDto::Session
        });
        builder = builder.with_mode(match mode {
            AhpModeDto::Session => AhpMode::Session,
            AhpModeDto::Automation => AhpMode::Automation,
        });
        if let Some(v) = &dto.provider {
            builder = builder.with_provider(mapper.resolve_string(v).await?);
        }
        if let Some(v) = &dto.session_config {
            builder = builder.with_session_config(v.clone());
        }
        if let Some(v) = &dto.title_template {
            builder = builder.with_title_template(v.clone());
        }
        for (query, route) in &dto.routes {
            builder = builder.with_route(query.clone(), query_config_from_dto(route));
        }
        if let Some(v) = &dto.default_template {
            builder = builder.with_default_template(query_config_from_dto(v));
        }
        if let Some(kind) = dto.follow_up_kind {
            builder = builder.with_follow_up_kind(match kind {
                FollowUpKindDto::Queued => FollowUpKind::Queued,
                FollowUpKindDto::Steering => FollowUpKind::Steering,
            });
        }
        if let Some(on_delete) = dto.on_delete {
            builder = builder.with_on_delete(match on_delete {
                OnDeleteDto::None => OnDelete::None,
                OnDeleteDto::Cancel => OnDelete::Cancel,
                OnDeleteDto::Archive => OnDelete::Archive,
                OnDeleteDto::Dispose => OnDelete::Dispose,
            });
        }
        if let Some(v) = &dto.durable {
            builder = builder.with_durable(mapper.resolve_typed(v).await?);
        }
        if let Some(v) = &dto.priority_queue_capacity {
            let resolved: u64 = mapper.resolve_typed(v).await?;
            let capacity = usize::try_from(resolved)
                .map_err(|_| anyhow::anyhow!("`priorityQueueCapacity` does not fit in usize"))?;
            builder = builder.with_priority_queue_capacity(capacity);
        }
        if let Some(policy) = dto.recovery_policy {
            builder = builder.with_recovery_policy(match policy {
                RecoveryPolicyDto::Strict => drasi_lib::recovery::ReactionRecoveryPolicy::Strict,
                RecoveryPolicyDto::AutoSkipGap => {
                    drasi_lib::recovery::ReactionRecoveryPolicy::AutoSkipGap
                }
            });
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());
        Ok(Box::new(reaction))
    }
}
