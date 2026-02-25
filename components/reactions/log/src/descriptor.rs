// Copyright 2025 The Drasi Authors.
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

//! Descriptor for the log reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::LogReactionBuilder;

/// DTO for a template specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = LogTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateSpecDto {
    /// Handlebars template string.
    #[serde(default)]
    pub template: String,
}

/// DTO for per-query template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = LogQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QueryConfigDto {
    /// Template for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpecDto>,

    /// Template for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpecDto>,

    /// Template for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpecDto>,
}

/// Configuration DTO for the log reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = LogReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct LogReactionConfigDto {
    /// Query-specific template configurations.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfigDto>,

    /// Default template configuration used when no query-specific route is defined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfigDto>,
}

fn map_template_spec(dto: &TemplateSpecDto) -> crate::TemplateSpec {
    crate::TemplateSpec::new(&dto.template)
}

fn map_query_config(dto: &QueryConfigDto) -> crate::QueryConfig {
    crate::QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(LogReactionConfigDto, QueryConfigDto, TemplateSpecDto,)))]
struct LogReactionSchemas;

/// Descriptor for the log reaction plugin.
pub struct LogReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for LogReactionDescriptor {
    fn kind(&self) -> &str {
        "log"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "LogReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = LogReactionSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().expect("OpenAPI components missing").schemas).expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: LogReactionConfigDto = serde_json::from_value(config_json.clone())?;

        let mut builder = LogReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref default_template) = dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
