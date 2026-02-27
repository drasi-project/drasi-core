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

//! Descriptor for the PostgreSQL stored procedure reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};

/// DTO for a template specification (stored procedure command).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::storedproc_postgres::StoredProcTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StoredProcTemplateSpecDto {
    /// Handlebars template string for the stored procedure command.
    pub template: String,
}

/// DTO for per-query stored procedure template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::storedproc_postgres::StoredProcQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StoredProcQueryConfigDto {
    /// Template for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<StoredProcTemplateSpecDto>,

    /// Template for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<StoredProcTemplateSpecDto>,

    /// Template for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<StoredProcTemplateSpecDto>,
}

/// Configuration DTO for the PostgreSQL stored procedure reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::storedproc_postgres::PostgresStoredProcReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct PostgresStoredProcReactionConfigDto {
    /// Database hostname or IP address.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub hostname: Option<ConfigValue<String>>,

    /// Database port.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU16>)]
    pub port: Option<ConfigValue<u16>>,

    /// Database user.
    #[schema(value_type = ConfigValueString)]
    pub user: ConfigValue<String>,

    /// Database password.
    #[schema(value_type = ConfigValueString)]
    pub password: ConfigValue<String>,

    /// Database name.
    #[schema(value_type = ConfigValueString)]
    pub database: ConfigValue<String>,

    /// Enable SSL/TLS.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub ssl: Option<ConfigValue<bool>>,

    /// Query-specific template configurations.
    #[serde(default)]
    pub routes: HashMap<String, StoredProcQueryConfigDto>,

    /// Default template configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<StoredProcQueryConfigDto>,

    /// Command timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub command_timeout_ms: Option<ConfigValue<u64>>,

    /// Number of retry attempts on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub retry_attempts: Option<ConfigValue<u32>>,
}

fn map_template_spec(dto: &StoredProcTemplateSpecDto) -> TemplateSpec {
    TemplateSpec::new(&dto.template)
}

fn map_query_config(dto: &StoredProcQueryConfigDto) -> QueryConfig {
    QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    PostgresStoredProcReactionConfigDto,
    StoredProcQueryConfigDto,
    StoredProcTemplateSpecDto,
)))]
struct PostgresStoredProcReactionSchemas;

/// Descriptor for the PostgreSQL stored procedure reaction plugin.
pub struct PostgresStoredProcReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for PostgresStoredProcReactionDescriptor {
    fn kind(&self) -> &str {
        "storedproc-postgres"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.storedproc_postgres.PostgresStoredProcReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = PostgresStoredProcReactionSchemas::openapi();
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
        let dto: PostgresStoredProcReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = PostgresStoredProcReaction::builder(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_user(mapper.resolve_string(&dto.user)?)
            .with_password(mapper.resolve_string(&dto.password)?)
            .with_database(mapper.resolve_string(&dto.database)?);

        if let Some(ref v) = dto.hostname {
            builder = builder.with_hostname(mapper.resolve_string(v)?);
        }
        if let Some(ref v) = dto.port {
            builder = builder.with_port(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.ssl {
            builder = builder.with_ssl(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.command_timeout_ms {
            builder = builder.with_command_timeout_ms(mapper.resolve_typed(v)?);
        }
        if let Some(ref v) = dto.retry_attempts {
            builder = builder.with_retry_attempts(mapper.resolve_typed(v)?);
        }

        if let Some(ref default_template) = dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        let reaction = builder.build().await?;
        Ok(Box::new(reaction))
    }
}
