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

use crate::config::{
    GitHubWorkGraphSourceConfig, LeaseTrust, TaskIssueType, TrustedIdentity, WebhookConfig,
    WorkerConfig, DEFAULT_BODY_LIMIT_BYTES, DEFAULT_WORKER_API_BASE_URL,
};
use crate::GitHubWorkGraphSourceBuilder;
use anyhow::{anyhow, Context};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::github_workgraph::GitHubWorkGraphSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubWorkGraphSourceConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub organization: ConfigValueString,
    pub task_issue_type: TaskIssueTypeDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub repositories: Vec<ConfigValueString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_config: Option<WorkerConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_trust: Option<LeaseTrustDto>,
    pub webhook: WebhookConfigDto,
    #[serde(default, with = "crate::config::DurabilityConfigDef")]
    #[schema(value_type = DurabilityConfigSchema)]
    pub durability: drasi_lib::DurabilityConfig,
}

/// Identities allowed to author lease lifecycle artifacts.
#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseTrustDto {
    pub dispatchers: Vec<TrustedIdentityDto>,
    pub reporters: Vec<TrustedIdentityDto>,
}

/// One trusted GitHub identity, matched on both node ID and login.
#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedIdentityDto {
    #[schema(value_type = ConfigValueString)]
    pub id: ConfigValueString,
    #[schema(value_type = ConfigValueString)]
    pub login: ConfigValueString,
}

/// Location and read-only credential of the worker-queue configuration file.
#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub repository: ConfigValueString,
    #[schema(value_type = ConfigValueString)]
    pub r#ref: ConfigValueString,
    #[schema(value_type = ConfigValueString)]
    pub path: ConfigValueString,
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValueString,
    #[serde(default = "default_worker_api_base_url")]
    #[schema(value_type = ConfigValueString)]
    pub api_base_url: ConfigValueString,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskIssueTypeDto {
    #[schema(value_type = ConfigValueString)]
    pub id: ConfigValueString,
    #[schema(value_type = ConfigValueString)]
    pub name: ConfigValueString,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookConfigDto {
    #[serde(default = "default_host")]
    #[schema(value_type = ConfigValueString)]
    pub host: ConfigValueString,
    #[serde(default = "default_port")]
    #[schema(value_type = ConfigValueU16)]
    pub port: ConfigValueU16,
    #[serde(default = "default_path")]
    #[schema(value_type = ConfigValueString)]
    pub path: ConfigValueString,
    #[schema(value_type = ConfigValueString)]
    pub secret: ConfigValueString,
    #[serde(default = "default_body_limit")]
    #[schema(value_type = ConfigValueUsize)]
    pub body_limit_bytes: ConfigValueUsize,
}

#[derive(utoipa::ToSchema)]
#[schema(rename_all = "camelCase")]
struct DurabilityConfigSchema {
    enabled: bool,
    max_events: u64,
    capacity_policy: CapacityPolicySchema,
}

#[derive(utoipa::ToSchema)]
enum CapacityPolicySchema {
    RejectIncoming,
}

fn default_host() -> ConfigValueString {
    ConfigValue::Static("0.0.0.0".to_string())
}
fn default_port() -> ConfigValueU16 {
    ConfigValue::Static(8080)
}
fn default_path() -> ConfigValueString {
    ConfigValue::Static("/webhook".to_string())
}
fn default_body_limit() -> ConfigValueUsize {
    ConfigValue::Static(DEFAULT_BODY_LIMIT_BYTES)
}
fn default_worker_api_base_url() -> ConfigValueString {
    ConfigValue::Static(DEFAULT_WORKER_API_BASE_URL.to_string())
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    GitHubWorkGraphSourceConfigDto,
    TaskIssueTypeDto,
    WorkerConfigDto,
    LeaseTrustDto,
    TrustedIdentityDto,
    WebhookConfigDto,
    ConfigValueStringSchema,
    ConfigValueU16Schema,
    ConfigValueU64Schema,
    ConfigValueUsizeSchema,
    DurabilityConfigSchema,
    CapacityPolicySchema,
)))]
struct GitHubWorkGraphSourceSchemas;

pub struct GitHubWorkGraphSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for GitHubWorkGraphSourceDescriptor {
    fn kind(&self) -> &str {
        "github-workgraph"
    }
    fn config_version(&self) -> &str {
        "2.0.0"
    }
    fn config_schema_name(&self) -> &str {
        "source.github_workgraph.GitHubWorkGraphSourceConfig"
    }
    fn config_schema_json(&self) -> String {
        let api = GitHubWorkGraphSourceSchemas::openapi();
        let components = api.components.as_ref().expect("OpenAPI components missing");
        serde_json::to_string(&components.schemas).expect("Failed to serialize config schema")
    }
    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: GitHubWorkGraphSourceConfigDto = serde_json::from_value(config_json.clone())
            .context("Invalid GitHub WorkGraph source configuration JSON")?;
        match &dto.webhook.secret {
            ConfigValue::Secret { name } if !name.trim().is_empty() => {}
            ConfigValue::Secret { .. } => anyhow::bail!("'webhook.secret' name cannot be empty"),
            _ => anyhow::bail!("'webhook.secret' must use SecretReference"),
        }
        let mapper = DtoMapper::new();
        let repositories = mapper.resolve_string_vec(&dto.repositories).await?;
        let worker_config = match &dto.worker_config {
            Some(worker) => {
                match &worker.token {
                    ConfigValue::Secret { name } if !name.trim().is_empty() => {}
                    ConfigValue::Secret { .. } => {
                        anyhow::bail!("'workerConfig.token' name cannot be empty")
                    }
                    _ => anyhow::bail!("'workerConfig.token' must use SecretReference"),
                }
                Some(WorkerConfig {
                    repository: mapper.resolve_string(&worker.repository).await?,
                    r#ref: mapper.resolve_string(&worker.r#ref).await?,
                    path: mapper.resolve_string(&worker.path).await?,
                    token: mapper.resolve_string(&worker.token).await?,
                    api_base_url: mapper.resolve_string(&worker.api_base_url).await?,
                })
            }
            None => None,
        };
        let lease_trust = match &dto.lease_trust {
            Some(trust) => {
                let mut resolved = Vec::new();
                for identities in [&trust.dispatchers, &trust.reporters] {
                    let mut out = Vec::with_capacity(identities.len());
                    for identity in identities {
                        out.push(TrustedIdentity {
                            id: mapper.resolve_string(&identity.id).await?,
                            login: mapper.resolve_string(&identity.login).await?,
                        });
                    }
                    resolved.push(out);
                }
                let reporters = resolved.pop().unwrap_or_default();
                let dispatchers = resolved.pop().unwrap_or_default();
                Some(LeaseTrust {
                    dispatchers,
                    reporters,
                })
            }
            None => None,
        };
        let config = GitHubWorkGraphSourceConfig {
            organization: mapper.resolve_string(&dto.organization).await?,
            task_issue_type: TaskIssueType {
                id: mapper.resolve_string(&dto.task_issue_type.id).await?,
                name: mapper.resolve_string(&dto.task_issue_type.name).await?,
            },
            repositories,
            worker_config,
            lease_trust,
            webhook: WebhookConfig {
                host: mapper.resolve_string(&dto.webhook.host).await?,
                port: mapper.resolve_typed(&dto.webhook.port).await?,
                path: mapper.resolve_string(&dto.webhook.path).await?,
                secret: mapper.resolve_string(&dto.webhook.secret).await?,
                body_limit_bytes: mapper.resolve_typed(&dto.webhook.body_limit_bytes).await?,
            },
            durability: dto.durability.clone(),
        };
        config.validate()?;
        let mut source = GitHubWorkGraphSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        source.base.set_raw_config(config_json.clone());
        Ok(Box::new(source))
    }
}
