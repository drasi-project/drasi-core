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

//! Descriptor for the Loki reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use log::{debug, info};
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::LokiReactionBuilder;

/// DTO for a log line template specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::loki::TemplateSpec)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSpecDto {
    /// Handlebars template for the log line.
    #[serde(default)]
    pub template: String,
}

/// DTO for per-query log line template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::loki::QueryConfig)]
#[serde(rename_all = "camelCase")]
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

/// DTO for basic authentication credentials.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::loki::BasicAuth)]
#[serde(rename_all = "camelCase")]
pub struct BasicAuthDto {
    /// Username for basic authentication.
    #[schema(value_type = ConfigValueString)]
    pub username: ConfigValue<String>,

    /// Password for basic authentication.
    #[schema(value_type = ConfigValueString)]
    pub password: ConfigValue<String>,
}

/// Configuration DTO for the Loki reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::loki::LokiReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct LokiReactionConfigDto {
    /// Loki push API endpoint URL.
    #[serde(default = "default_endpoint")]
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    /// Static labels applied to all log streams (values support Handlebars templates).
    #[serde(default)]
    pub labels: HashMap<String, String>,

    /// Optional Loki tenant ID (X-Scope-OrgID header).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub tenant_id: Option<ConfigValue<String>>,

    /// Optional bearer token for authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub token: Option<ConfigValue<String>>,

    /// Optional basic authentication credentials.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<BasicAuthDto>,

    /// HTTP request timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    /// Per-query log line template configuration.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfigDto>,

    /// Default template used when no query-specific route matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfigDto>,
}

fn default_endpoint() -> ConfigValue<String> {
    ConfigValue::Static("http://localhost:3100".to_string())
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
#[openapi(components(schemas(
    LokiReactionConfigDto,
    QueryConfigDto,
    TemplateSpecDto,
    BasicAuthDto,
)))]
struct LokiReactionSchemas;

/// Descriptor for the Loki reaction plugin.
pub struct LokiReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for LokiReactionDescriptor {
    fn kind(&self) -> &str {
        "loki"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.loki.LokiReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = LokiReactionSchemas::openapi();
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
        debug!(
            "[{id}] creating Loki reaction - queries: {}, auto_start: {auto_start}",
            query_ids.len()
        );
        let dto: LokiReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = LokiReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint)?);

        for (key, value) in &dto.labels {
            builder = builder.with_label(key, value);
        }

        if let Some(ref tenant_id) = dto.tenant_id {
            builder = builder.with_tenant_id(mapper.resolve_string(tenant_id)?);
        }

        if let Some(ref token) = dto.token {
            builder = builder.with_token(mapper.resolve_string(token)?);
        }

        if let Some(ref basic_auth) = dto.basic_auth {
            builder = builder.with_basic_auth(
                mapper.resolve_string(&basic_auth.username)?,
                mapper.resolve_string(&basic_auth.password)?,
            );
        }

        if let Some(ref timeout_ms) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(timeout_ms)?);
        }

        for (query_id, config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(config));
        }

        if let Some(ref default_template) = dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }

        debug!(
            "[{id}] config parsed - endpoint: (configured), labels: {}, routes: {}, auth_configured: {}",
            dto.labels.len(),
            dto.routes.len(),
            dto.token.is_some() || dto.basic_auth.is_some()
        );

        let reaction = builder.build()?;
        info!("[{id}] Loki reaction created successfully");
        Ok(Box::new(reaction))
    }
}
