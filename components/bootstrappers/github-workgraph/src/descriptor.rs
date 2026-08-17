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

//! GitHub WorkGraph bootstrap plugin descriptor and configuration DTO.

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use drasi_source_github_workgraph::descriptor::GitHubWorkGraphSourceConfigDto;
use utoipa::OpenApi;

use crate::GitHubWorkGraphBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

fn default_api_base_url() -> ConfigValue<String> {
    ConfigValue::Static(crate::config::DEFAULT_API_BASE_URL.to_string())
}

fn default_max_concurrency() -> ConfigValue<usize> {
    ConfigValue::Static(crate::config::DEFAULT_MAX_CONCURRENCY)
}

/// GitHub WorkGraph bootstrap configuration DTO.
///
/// The organization and repository allowlist come from the parent
/// `github-workgraph` Source configuration so bootstrap and streaming always
/// target the same graph.
/// `token` **must** be a read-only credential (a fine-grained PAT scoped to
/// `Issues: Read`, `Pull requests: Read`, `Metadata: Read` on the target
/// organization, or an equivalent read-only GitHub App installation token).
/// This bootstrapper never writes to GitHub.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::github_workgraph::GitHubWorkGraphBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubWorkGraphBootstrapConfigDto {
    /// A read-only GitHub token (PAT or GitHub App installation token).
    /// Use a `Secret` reference in production; never inline a live token.
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValue<String>,
    /// GraphQL API endpoint. Override for GitHub Enterprise Server, e.g.
    /// `https://github.example.com/api/graphql`.
    #[serde(default = "default_api_base_url")]
    #[schema(value_type = ConfigValueString)]
    pub api_base_url: ConfigValue<String>,
    /// Upper bound on concurrently in-flight GraphQL requests, and on the
    /// number of repositories processed concurrently.
    #[serde(default = "default_max_concurrency")]
    #[schema(value_type = ConfigValueUsize)]
    pub max_concurrency: ConfigValue<usize>,
}

// ── Descriptor ───────────────────────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(GitHubWorkGraphBootstrapConfigDto)))]
struct GitHubWorkGraphBootstrapSchemas;

/// Plugin descriptor for the GitHub WorkGraph bootstrap provider.
pub struct GitHubWorkGraphBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for GitHubWorkGraphBootstrapDescriptor {
    fn kind(&self) -> &str {
        "github-workgraph"
    }

    fn config_version(&self) -> &str {
        "2.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.github_workgraph.GitHubWorkGraphBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = GitHubWorkGraphBootstrapSchemas::openapi();
        match api
            .components
            .as_ref()
            .and_then(|components| serde_json::to_string(&components.schemas).ok())
        {
            Some(schema) => schema,
            None => {
                log::warn!(
                    "GitHub WorkGraph bootstrap schema generation failed; returning empty schema"
                );
                "{}".to_string()
            }
        }
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: GitHubWorkGraphBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let source_dto: GitHubWorkGraphSourceConfigDto =
            serde_json::from_value(source_config_json.clone())?;
        let mapper = DtoMapper::new();
        let repositories = mapper.resolve_string_vec(&source_dto.repositories).await?;

        let provider = GitHubWorkGraphBootstrapProvider::builder()
            .with_organization(mapper.resolve_string(&source_dto.organization).await?)
            .with_task_issue_type(drasi_source_github_workgraph::config::TaskIssueType {
                id: mapper
                    .resolve_string(&source_dto.task_issue_type.id)
                    .await?,
                name: mapper
                    .resolve_string(&source_dto.task_issue_type.name)
                    .await?,
            })
            .with_repositories(repositories)
            .with_token(mapper.resolve_string(&dto.token).await?)
            .with_api_base_url(mapper.resolve_string(&dto.api_base_url).await?)
            .with_max_concurrency(mapper.resolve_typed(&dto.max_concurrency).await?)
            .build()?;
        Ok(Box::new(provider))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn descriptor_reads_scope_from_source_config() {
        let descriptor = GitHubWorkGraphBootstrapDescriptor;
        let provider = descriptor
            .create_bootstrap_provider(
                &json!({ "token": "read-only-token" }),
                &json!({
                    "organization": "acme",
                    "taskIssueType": {"id":"IT_test","name":"WorkGraphTask"},
                    "repositories": ["widgets", "acme/widgets"],
                    "webhook": { "secret": "webhook-secret" }
                }),
            )
            .await;

        assert!(provider.is_ok());
        let schema = descriptor.config_schema_json();
        assert!(schema.contains("\"token\""));
        assert!(!schema.contains("\"organization\""));
        assert!(!schema.contains("\"repositories\""));

        let foreign = descriptor
            .create_bootstrap_provider(
                &json!({ "token": "read-only-token" }),
                &json!({
                    "organization": "acme",
                    "taskIssueType": {"id":"IT_test","name":"WorkGraphTask"},
                    "repositories": ["other/widgets"],
                    "webhook": { "secret": "webhook-secret" }
                }),
            )
            .await;
        assert!(foreign.is_err());
    }
}
