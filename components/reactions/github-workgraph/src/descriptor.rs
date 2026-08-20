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

use crate::config::DEFAULT_API_BASE_URL;
use crate::{GitHubWorkGraphReaction, GitHubWorkGraphReactionConfig};

fn default_api_base_url() -> ConfigValue<String> {
    ConfigValue::Static(DEFAULT_API_BASE_URL.to_string())
}

#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::github_workgraph::GitHubWorkGraphReactionConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubWorkGraphReactionConfigDto {
    /// GitHub token with permission to create issue comments.
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValue<String>,

    /// GitHub REST API base URL.
    #[serde(default = "default_api_base_url")]
    #[schema(value_type = ConfigValueString)]
    pub api_base_url: ConfigValue<String>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(GitHubWorkGraphReactionConfigDto)))]
struct GitHubWorkGraphReactionSchemas;

pub struct GitHubWorkGraphReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for GitHubWorkGraphReactionDescriptor {
    fn kind(&self) -> &str {
        "github-workgraph-dispatcher"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.github_workgraph.GitHubWorkGraphReactionConfig"
    }

    fn display_name(&self) -> &str {
        "GitHub WorkGraph Dispatcher"
    }

    fn display_description(&self) -> &str {
        "Dispatches WorkGraph tasks to available worker slots."
    }

    fn display_icon(&self) -> &str {
        "github"
    }

    fn config_schema_json(&self) -> String {
        let api = GitHubWorkGraphReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("failed to serialize config schema");

        drasi_plugin_sdk::schema_ui::SchemaUiAnnotator::new(
            schemas,
            "reaction.github_workgraph.GitHubWorkGraphReactionConfig",
        )
        .expect("root schema not found")
        .field("token", |field| field.order(1).widget("password"))
        .field("apiBaseUrl", |field| field.order(2))
        .annotate()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: GitHubWorkGraphReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();
        let config = GitHubWorkGraphReactionConfig::new(mapper.resolve_string(&dto.token).await?)
            .with_api_base_url(mapper.resolve_string(&dto.api_base_url).await?);

        let mut reaction = GitHubWorkGraphReaction::new(id, query_ids, config, auto_start)?;
        reaction.base.set_raw_config(config_json.clone());
        Ok(Box::new(reaction))
    }
}
