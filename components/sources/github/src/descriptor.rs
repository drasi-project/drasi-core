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

//! GitHub source plugin descriptor and DTO mapping.

use crate::config::{GitHubSourceConfig, ProjectSpec, WebhookConfig};
use crate::GitHubSourceBuilder;
use anyhow::{anyhow, Context};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// GitHub source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::github::GitHubSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubSourceConfigDto {
    pub token: ConfigValueString,
    #[serde(default)]
    pub repositories: Vec<ConfigValueString>,
    #[serde(default)]
    pub projects: Vec<ProjectSpecDto>,
    pub webhook: WebhookConfigDto,
    #[serde(default = "default_reconcile_interval")]
    pub reconcile_interval_secs: ConfigValueU64,
    #[serde(default)]
    pub durability: drasi_lib::DurabilityConfig,
    #[serde(default = "default_graphql_url")]
    pub graphql_url: ConfigValueString,
    #[serde(default)]
    pub skip_initial_bootstrap: bool,
}

fn default_reconcile_interval() -> ConfigValueU64 {
    ConfigValue::Static(300)
}

fn default_graphql_url() -> ConfigValueString {
    ConfigValue::Static("https://api.github.com/graphql".to_string())
}

/// Project selector DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::github::ProjectSpec)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSpecDto {
    pub owner: ConfigValueString,
    pub number: ConfigValueU32,
}

/// Webhook listener DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::github::WebhookConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookConfigDto {
    #[serde(default = "default_host")]
    pub host: ConfigValueString,
    #[serde(default = "default_port")]
    pub port: ConfigValueU16,
    #[serde(default = "default_path")]
    pub path: ConfigValueString,
    pub secret: ConfigValueString,
    #[serde(default = "default_body_limit")]
    pub body_limit_bytes: ConfigValueUsize,
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
    ConfigValue::Static(10 * 1024 * 1024)
}

#[derive(OpenApi)]
#[openapi(components(schemas(GitHubSourceConfigDto)))]
struct GitHubSourceSchemas;

/// Descriptor for authorized GitHub source.
pub struct GitHubSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for GitHubSourceDescriptor {
    fn kind(&self) -> &str {
        "github"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.github.GitHubSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GitHubSourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: GitHubSourceConfigDto = serde_json::from_value(config_json.clone())
            .context("Invalid GitHub source configuration JSON")?;

        require_secret_config(&dto.token, "token")
            .context("GitHub PAT must be configured as SecretReference")?;
        require_secret_config(&dto.webhook.secret, "webhook.secret")
            .context("Webhook secret must be configured as SecretReference")?;

        let mapper = DtoMapper::new();
        let repositories = resolve_repositories(&mapper, &dto.repositories).await?;

        let mut projects = Vec::with_capacity(dto.projects.len());
        for project in &dto.projects {
            projects.push(ProjectSpec {
                owner: mapper.resolve_string(&project.owner).await?,
                number: mapper.resolve_typed(&project.number).await?,
            });
        }

        let config = GitHubSourceConfig {
            token: mapper.resolve_string(&dto.token).await?,
            repositories,
            projects,
            webhook: WebhookConfig {
                host: mapper.resolve_string(&dto.webhook.host).await?,
                port: mapper.resolve_typed(&dto.webhook.port).await?,
                path: mapper.resolve_string(&dto.webhook.path).await?,
                secret: mapper.resolve_string(&dto.webhook.secret).await?,
                body_limit_bytes: mapper.resolve_typed(&dto.webhook.body_limit_bytes).await?,
            },
            reconcile_interval_secs: mapper.resolve_typed(&dto.reconcile_interval_secs).await?,
            durability: dto.durability.clone(),
            graphql_url: mapper.resolve_string(&dto.graphql_url).await?,
            skip_initial_bootstrap: dto.skip_initial_bootstrap,
        };

        config.validate()?;

        let mut source = GitHubSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        source.base.set_raw_config(config_json.clone());

        Ok(Box::new(source))
    }
}

fn require_secret_config(value: &ConfigValue<String>, field: &str) -> anyhow::Result<()> {
    match value {
        ConfigValue::Secret { name } if !name.trim().is_empty() => Ok(()),
        ConfigValue::Secret { .. } => Err(anyhow!("'{field}' secret name cannot be empty")),
        _ => Err(anyhow!("'{field}' must use SecretReference")),
    }
}

async fn resolve_repositories(
    mapper: &DtoMapper,
    repositories: &[ConfigValue<String>],
) -> anyhow::Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(repositories.len());
    for repo in repositories {
        resolved.push(mapper.resolve_string(repo).await?);
    }
    Ok(resolved)
}
