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

//! Descriptor for the shell reaction plugin.

#![cfg(target_os = "linux")]

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::config::ShellExtension;
use crate::ShellReactionBuilder;

/// DTO for a named shell command.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::shell::ShellCommand)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ShellCommandDto {
    /// Executable path or name.
    pub executable: String,

    /// Arguments to pass to the executable.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub args: Vec<String>,
}

/// DTO for a shell template specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::shell::ShellTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ShellTemplateSpecDto {
    /// Handlebars template rendered to stdin of the command.
    #[serde(default)]
    pub template: String,

    /// Per-invocation environment variables merged with global env.
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub env: HashMap<String, String>,
}

/// DTO for per-query shell template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::shell::ShellQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ShellQueryConfigDto {
    /// Template for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<ShellTemplateSpecDto>,

    /// Template for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<ShellTemplateSpecDto>,

    /// Template for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<ShellTemplateSpecDto>,
}

/// Configuration DTO for the shell reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::shell::ShellReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct ShellReactionConfigDto {
    /// Maximum number of concurrent shell commands. Default: 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,

    /// Maximum bytes to send to stdin. Default: 1 MB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_stdin_bytes: Option<usize>,

    /// Maximum bytes to capture from stdout/stderr. Default: 4 KB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_limit: Option<usize>,

    /// Command timeout in seconds. Default: 60.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,

    /// Kill the command if the reaction is dropped. Default: true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill_on_drop: Option<bool>,

    /// Maximum number of recent invocations to retain. Default: 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_recent_invocations: Option<u32>,

    /// Global environment variables applied to all commands.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Named shell command definitions.
    #[serde(default)]
    pub commands: HashMap<String, ShellCommandDto>,

    /// Query-specific template configurations.
    #[serde(default)]
    pub routes: HashMap<String, ShellQueryConfigDto>,

    /// Default template used when no query-specific route is defined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<ShellQueryConfigDto>,
}

fn map_template_spec(dto: &ShellTemplateSpecDto) -> crate::config::TemplateSpec<ShellExtension> {
    crate::config::TemplateSpec {
        template: dto.template.clone(),
        extension: ShellExtension {
            env: dto.env.clone(),
        },
    }
}

fn map_query_config(dto: &ShellQueryConfigDto) -> crate::config::QueryConfig<ShellExtension> {
    crate::config::QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    ShellReactionConfigDto,
    ShellQueryConfigDto,
    ShellTemplateSpecDto,
    ShellCommandDto,
)))]
struct ShellReactionSchemas;

/// Descriptor for the shell reaction plugin.
pub struct ShellReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for ShellReactionDescriptor {
    fn kind(&self) -> &str {
        "shell"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.shell.ShellReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = ShellReactionSchemas::openapi();
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
        let dto: ShellReactionConfigDto = serde_json::from_value(config_json.clone())?;

        let mut builder = ShellReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_envs(dto.env);

        if let Some(v) = dto.max_concurrent {
            builder = builder.with_max_concurrent(v);
        }
        if let Some(v) = dto.max_stdin_bytes {
            builder = builder.with_max_stdin_bytes(v);
        }
        if let Some(v) = dto.capture_limit {
            builder = builder.with_capture_limit(v);
        }
        if let Some(v) = dto.timeout_s {
            builder = builder.with_timeout_s(v);
        }
        if let Some(v) = dto.kill_on_drop {
            builder = builder.with_kill_on_drop(v);
        }
        if let Some(v) = dto.max_recent_invocations {
            builder = builder.with_max_recent_invocations(v);
        }

        for (name, cmd) in dto.commands {
            builder = builder.with_command(
                name,
                crate::config::ShellCommand {
                    executable: cmd.executable,
                    args: cmd.args,
                },
            );
        }

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
