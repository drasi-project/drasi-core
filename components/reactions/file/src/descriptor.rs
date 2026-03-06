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

//! Descriptor for the file reaction plugin.

use crate::{FileReactionBuilder, WriteMode};
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

/// DTO for a template specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::file::FileTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct TemplateSpecDto {
    /// Handlebars template string.
    #[serde(default)]
    pub template: String,
}

/// DTO for per-query template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::file::FileQueryConfig)]
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

/// DTO for write mode.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::file::WriteMode)]
#[serde(rename_all = "snake_case")]
pub enum WriteModeDto {
    Append,
    Overwrite,
    PerChange,
}

/// Configuration DTO for the file reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::file::FileReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct FileReactionConfigDto {
    /// Base directory for generated files.
    #[schema(value_type = ConfigValueString)]
    pub output_path: ConfigValue<String>,
    /// File write mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_mode: Option<WriteModeDto>,
    /// Handlebars filename template.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub filename_template: Option<ConfigValue<String>>,
    /// Query-specific templates.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfigDto>,
    /// Default templates when route-specific templates are missing.
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

fn map_write_mode(dto: WriteModeDto) -> WriteMode {
    match dto {
        WriteModeDto::Append => WriteMode::Append,
        WriteModeDto::Overwrite => WriteMode::Overwrite,
        WriteModeDto::PerChange => WriteMode::PerChange,
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    FileReactionConfigDto,
    QueryConfigDto,
    TemplateSpecDto,
    WriteModeDto
)))]
struct FileReactionSchemas;

/// Descriptor for the file reaction plugin.
pub struct FileReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for FileReactionDescriptor {
    fn kind(&self) -> &str {
        "file"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.file.FileReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = FileReactionSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: FileReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = FileReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_output_path(mapper.resolve_string(&dto.output_path)?);

        if let Some(write_mode) = dto.write_mode {
            builder = builder.with_write_mode(map_write_mode(write_mode));
        }

        if let Some(filename_template) = dto.filename_template {
            builder = builder.with_filename_template(mapper.resolve_string(&filename_template)?);
        }

        if let Some(default_template) = &dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
