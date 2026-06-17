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

//! Descriptor for the HTTP reaction plugin.

use std::collections::HashMap;

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{
    AdaptiveBatchConfig, HttpCallExt, HttpOutputTemplates, HttpQueryConfig, HttpReactionConfig,
};
use crate::output::{BatchEnvelope, DefaultChangeNotification, Operation};
use crate::HttpReactionBuilder;

// ---------------------------------------------------------------------------
// DTO: per-operation HTTP call spec
// ---------------------------------------------------------------------------

/// DTO for a single HTTP call spec. Mirrors `HttpCallSpec` on the wire:
/// a Handlebars `template` (body) plus `url`, `method`, and `headers`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::http::HttpCallSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpCallSpecDto {
    /// Request body as a Handlebars template. Empty = the default
    /// `DefaultChangeNotification` envelope.
    #[serde(default)]
    pub template: String,

    /// URL path (appended to `baseUrl`) or absolute URL. Supports Handlebars.
    #[serde(default)]
    pub url: String,

    /// HTTP method (GET/POST/PUT/DELETE/PATCH). Defaults to POST.
    #[serde(default)]
    pub method: String,

    /// Additional HTTP headers. Header values support Handlebars.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// DTO for per-query call configuration: one [`HttpCallSpecDto`] per operation.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::http::HttpQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpQueryConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<HttpCallSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<HttpCallSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<HttpCallSpecDto>,
}

/// DTO mirroring [`HttpOutputTemplates`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::http::HttpOutputTemplates)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HttpOutputTemplatesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<HttpQueryConfigDto>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, HttpQueryConfigDto>,
}

/// DTO mirroring [`AdaptiveBatchConfig`].
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::http::AdaptiveBatchConfig)]
#[serde(rename_all = "camelCase")]
pub struct AdaptiveBatchConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub adaptive_min_batch_size: Option<ConfigValue<u64>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub adaptive_max_batch_size: Option<ConfigValue<u64>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub adaptive_window_size: Option<ConfigValue<u64>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub adaptive_batch_timeout_ms: Option<ConfigValue<u64>>,
}

/// Top-level HTTP reaction config DTO.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::http::HttpReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct HttpReactionConfigDto {
    /// Base URL for HTTP requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub base_url: Option<ConfigValue<String>>,

    /// Optional authentication token (sent as `Authorization: Bearer …`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub token: Option<ConfigValue<String>>,

    /// Request timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    /// Priority queue capacity for inbound query results.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,

    /// Per-query and default output templates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<HttpOutputTemplatesDto>,

    /// When set, enables adaptive batching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<AdaptiveBatchConfigDto>,

    /// When set, coalesced batches POST to `{baseUrl}{batchEndpoint}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub batch_endpoint: Option<ConfigValue<String>>,
}

// ---------------------------------------------------------------------------
// DTO → domain conversion
// ---------------------------------------------------------------------------

fn map_call_spec(dto: &HttpCallSpecDto) -> crate::config::HttpCallSpec {
    crate::config::HttpCallSpec {
        template: dto.template.clone(),
        extension: HttpCallExt {
            url: dto.url.clone(),
            method: dto.method.clone(),
            headers: dto.headers.clone(),
        },
    }
}

fn map_query_config(dto: &HttpQueryConfigDto) -> HttpQueryConfig {
    HttpQueryConfig {
        added: dto.added.as_ref().map(map_call_spec),
        updated: dto.updated.as_ref().map(map_call_spec),
        deleted: dto.deleted.as_ref().map(map_call_spec),
    }
}

fn map_output_templates(dto: &HttpOutputTemplatesDto) -> HttpOutputTemplates {
    HttpOutputTemplates {
        default_template: dto.default_template.as_ref().map(map_query_config),
        routes: dto
            .routes
            .iter()
            .map(|(k, v)| (k.clone(), map_query_config(v)))
            .collect(),
    }
}

// ---------------------------------------------------------------------------
// Domain → DTO conversion (used by properties())
// ---------------------------------------------------------------------------

fn dto_call_spec(spec: &crate::config::HttpCallSpec) -> HttpCallSpecDto {
    HttpCallSpecDto {
        template: spec.template.clone(),
        url: spec.extension.url.clone(),
        method: spec.extension.method.clone(),
        headers: spec.extension.headers.clone(),
    }
}

fn dto_query_config(qc: &HttpQueryConfig) -> HttpQueryConfigDto {
    HttpQueryConfigDto {
        added: qc.added.as_ref().map(dto_call_spec),
        updated: qc.updated.as_ref().map(dto_call_spec),
        deleted: qc.deleted.as_ref().map(dto_call_spec),
    }
}

fn dto_output_templates(t: &HttpOutputTemplates) -> HttpOutputTemplatesDto {
    HttpOutputTemplatesDto {
        default_template: t.default_template.as_ref().map(dto_query_config),
        routes: t
            .routes
            .iter()
            .map(|(k, v)| (k.clone(), dto_query_config(v)))
            .collect(),
    }
}

fn dto_adaptive(a: &AdaptiveBatchConfig) -> AdaptiveBatchConfigDto {
    AdaptiveBatchConfigDto {
        adaptive_min_batch_size: Some(ConfigValue::Static(a.adaptive_min_batch_size as u64)),
        adaptive_max_batch_size: Some(ConfigValue::Static(a.adaptive_max_batch_size as u64)),
        adaptive_window_size: Some(ConfigValue::Static(a.adaptive_window_size as u64)),
        adaptive_batch_timeout_ms: Some(ConfigValue::Static(a.adaptive_batch_timeout_ms)),
    }
}

impl From<&HttpReactionConfig> for HttpReactionConfigDto {
    fn from(c: &HttpReactionConfig) -> Self {
        Self {
            base_url: Some(ConfigValue::Static(c.base_url.clone())),
            token: c.token.as_ref().map(|t| ConfigValue::Static(t.clone())),
            timeout_ms: Some(ConfigValue::Static(c.timeout_ms)),
            priority_queue_capacity: None,
            output_templates: c.output_templates.as_ref().map(dto_output_templates),
            adaptive: c.adaptive.as_ref().map(dto_adaptive),
            batch_endpoint: c
                .batch_endpoint
                .as_ref()
                .map(|e| ConfigValue::Static(e.clone())),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI schemas
// ---------------------------------------------------------------------------

#[derive(OpenApi)]
#[openapi(components(schemas(
    HttpReactionConfigDto,
    HttpOutputTemplatesDto,
    HttpQueryConfigDto,
    HttpCallSpecDto,
    AdaptiveBatchConfigDto,
)))]
struct HttpReactionSchemas;

/// OpenAPI grouping for the **output** wire-format types emitted by the
/// HTTP reaction (the [`DefaultChangeNotification`] envelope and the
/// Pattern C [`BatchEnvelope`] container). Generated as the source of
/// truth for `schema/output.schema.json`.
#[derive(OpenApi)]
#[openapi(components(schemas(DefaultChangeNotification, Operation, BatchEnvelope,)))]
pub struct HttpReactionOutputSchemas;

impl HttpReactionOutputSchemas {
    /// Render the output schemas as a JSON Schema document — the same
    /// shape that is committed to `schema/output.schema.json`. Top-level
    /// object has a `$schema` declaration plus the `components.schemas`
    /// map keyed by the `schema(as = ...)` names declared on each type.
    pub fn as_json_schema() -> serde_json::Value {
        let api = Self::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize output schemas");
        serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "$id": "https://drasi.io/schemas/reactions/http/output.schema.json",
            "title": "Drasi HTTP Reaction — Default Output Schemas",
            "description": "JSON Schemas for the wire-format payloads emitted by drasi-reaction-http.",
            "components": { "schemas": schemas },
        })
    }
}

/// Descriptor for the HTTP reaction plugin.
pub struct HttpReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for HttpReactionDescriptor {
    fn kind(&self) -> &str {
        "http"
    }

    fn config_version(&self) -> &str {
        "2.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.http.HttpReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = HttpReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");

        SchemaUiAnnotator::new(schemas, "reaction.http.HttpReactionConfig")
            .expect("root schema not found")
            .field("baseUrl", |f| {
                f.group("Connection")
                    .order(1)
                    .placeholder("https://api.example.com")
            })
            .field("token", |f| {
                f.group("Connection").order(2).widget("password")
            })
            .field("timeoutMs", |f| {
                f.group("Connection").order(3).placeholder("5000")
            })
            .field("priorityQueueCapacity", |f| {
                f.group("Advanced").order(10).placeholder("10000")
            })
            .field("outputTemplates", |f| f.group("Templates").order(20))
            .field("adaptive", |f| f.group("Adaptive Batching").order(30))
            .field("batchEndpoint", |f| {
                f.group("Adaptive Batching").order(31).placeholder("/batch")
            })
            .annotate()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: HttpReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = HttpReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref base_url) = dto.base_url {
            builder = builder.with_base_url(mapper.resolve_string(base_url).await?);
        }
        if let Some(ref token) = dto.token {
            builder = builder.with_token(mapper.resolve_string(token).await?);
        }
        if let Some(ref timeout_ms) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(timeout_ms).await?);
        }
        if let Some(ref cap) = dto.priority_queue_capacity {
            let resolved: u64 = mapper.resolve_typed(cap).await?;
            builder = builder.with_priority_queue_capacity(resolved as usize);
        }
        if let Some(ref templates) = dto.output_templates {
            builder = builder.with_output_templates(map_output_templates(templates));
        }
        if let Some(ref adaptive) = dto.adaptive {
            let cfg = AdaptiveBatchConfig {
                adaptive_min_batch_size: if let Some(ref v) = adaptive.adaptive_min_batch_size {
                    mapper.resolve_typed::<u64>(v).await? as usize
                } else {
                    AdaptiveBatchConfig::default().adaptive_min_batch_size
                },
                adaptive_max_batch_size: if let Some(ref v) = adaptive.adaptive_max_batch_size {
                    mapper.resolve_typed::<u64>(v).await? as usize
                } else {
                    AdaptiveBatchConfig::default().adaptive_max_batch_size
                },
                adaptive_window_size: if let Some(ref v) = adaptive.adaptive_window_size {
                    mapper.resolve_typed::<u64>(v).await? as usize
                } else {
                    AdaptiveBatchConfig::default().adaptive_window_size
                },
                adaptive_batch_timeout_ms: if let Some(ref v) = adaptive.adaptive_batch_timeout_ms {
                    mapper.resolve_typed::<u64>(v).await?
                } else {
                    AdaptiveBatchConfig::default().adaptive_batch_timeout_ms
                },
            };
            builder = builder.with_adaptive(cfg);
        }
        if let Some(ref ep) = dto.batch_endpoint {
            builder = builder.with_batch_endpoint(mapper.resolve_string(ep).await?);
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}
