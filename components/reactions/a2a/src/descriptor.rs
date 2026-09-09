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

use crate::{A2AReactionBuilder, A2AReactionConfig, TerminalUpdatePolicy};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = reaction::a2a::TerminalUpdatePolicy)]
#[serde(rename_all = "snake_case")]
pub enum TerminalUpdatePolicyDto {
    Replace,
    Ignore,
}

impl From<TerminalUpdatePolicyDto> for TerminalUpdatePolicy {
    fn from(value: TerminalUpdatePolicyDto) -> Self {
        match value {
            TerminalUpdatePolicyDto::Replace => Self::Replace,
            TerminalUpdatePolicyDto::Ignore => Self::Ignore,
        }
    }
}

impl From<TerminalUpdatePolicy> for TerminalUpdatePolicyDto {
    fn from(value: TerminalUpdatePolicy) -> Self {
        match value {
            TerminalUpdatePolicy::Replace => Self::Replace,
            TerminalUpdatePolicy::Ignore => Self::Ignore,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::a2a::RecoveryPolicy)]
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

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::a2a::A2AReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct A2AReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub token: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_key_fields: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub instruction_template: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_update_policy: Option<TerminalUpdatePolicyDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub return_immediately: Option<ConfigValue<bool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<RecoveryPolicyDto>,
}

impl From<&A2AReactionConfig> for A2AReactionConfigDto {
    fn from(config: &A2AReactionConfig) -> Self {
        Self {
            endpoint: ConfigValue::Static(config.endpoint.clone()),
            token: config.token.clone().map(ConfigValue::Static),
            timeout_ms: Some(ConfigValue::Static(config.timeout_ms)),
            result_key_fields: config.result_key_fields.clone(),
            instruction_template: config.instruction_template.clone().map(ConfigValue::Static),
            terminal_update_policy: Some(config.terminal_update_policy.into()),
            return_immediately: Some(ConfigValue::Static(config.return_immediately)),
            priority_queue_capacity: None,
            recovery_policy: None,
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(A2AReactionConfigDto, RecoveryPolicyDto, TerminalUpdatePolicyDto)))]
struct A2AReactionSchemas;

pub struct A2AReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for A2AReactionDescriptor {
    fn kind(&self) -> &str {
        "a2a"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.a2a.A2AReactionConfig"
    }

    fn display_name(&self) -> &str {
        "A2A"
    }

    fn display_description(&self) -> &str {
        "Delivers Drasi query-result diffs to A2A JSON-RPC endpoints."
    }

    fn display_icon(&self) -> &str {
        "link"
    }

    fn config_schema_json(&self) -> String {
        let api = A2AReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize config schema");

        drasi_plugin_sdk::schema_ui::SchemaUiAnnotator::new(
            schemas,
            "reaction.a2a.A2AReactionConfig",
        )
        .expect("root schema not found")
        .field("endpoint", |field| {
            field
                .group("Connection")
                .order(1)
                .placeholder("https://agentgateway.example.com/")
        })
        .field("token", |field| {
            field.group("Connection").order(2).widget("password")
        })
        .field("timeoutMs", |field| {
            field.group("Connection").order(3).placeholder("5000")
        })
        .field("resultKeyFields", |field| field.group("Routing").order(10))
        .field("instructionTemplate", |field| {
            field.group("Message").order(20)
        })
        .field("terminalUpdatePolicy", |field| {
            field.group("Message").order(21)
        })
        .field("returnImmediately", |field| {
            field.group("Message").order(22)
        })
        .field("priorityQueueCapacity", |field| {
            field.group("Advanced").order(30).placeholder("10000")
        })
        .field("recoveryPolicy", |field| field.group("Advanced").order(31))
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
        let dto: A2AReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = A2AReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint).await?)
            .with_result_key_fields(dto.result_key_fields.clone());

        if let Some(token) = &dto.token {
            builder = builder.with_token(mapper.resolve_string(token).await?);
        }
        if let Some(timeout_ms) = &dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(timeout_ms).await?);
        }
        if let Some(template) = &dto.instruction_template {
            builder = builder.with_instruction_template(mapper.resolve_string(template).await?);
        }
        if let Some(policy) = dto.terminal_update_policy {
            builder = builder.with_terminal_update_policy(policy.into());
        }
        if let Some(return_immediately) = &dto.return_immediately {
            builder =
                builder.with_return_immediately(mapper.resolve_typed(return_immediately).await?);
        }
        if let Some(capacity) = &dto.priority_queue_capacity {
            let resolved: u64 = mapper.resolve_typed(capacity).await?;
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
