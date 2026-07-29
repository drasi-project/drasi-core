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

//! Descriptor for the Event Grid reaction plugin.

use std::collections::HashMap;

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{
    EventGridOutputTemplates, EventGridQueryConfig, EventGridReactionConfig, EventGridSchema,
    EventGridTemplateExt, EventGridTemplateSpec, OutputFormat,
};
use crate::EventGridReaction;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// DTO for a single templated event spec: a `data` body template + `metadata`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::eventgrid::EventGridTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridTemplateSpecDto {
    /// Handlebars template producing the event `data` (JSON).
    #[serde(default)]
    pub template: String,

    /// CloudEvent extension attributes (CloudEvents schema only).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// DTO for per-query template configuration: one spec per operation.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::eventgrid::EventGridQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridQueryConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<EventGridTemplateSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<EventGridTemplateSpecDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<EventGridTemplateSpecDto>,
}

/// DTO mirroring [`EventGridOutputTemplates`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::eventgrid::EventGridOutputTemplates)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridOutputTemplatesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<EventGridQueryConfigDto>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, EventGridQueryConfigDto>,
}

/// Top-level Event Grid reaction config DTO.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::eventgrid::EventGridReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridReactionConfigDto {
    /// Full Event Grid custom-topic events URL.
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    /// Topic access key (sent as `aeg-sas-key`). Omit to use AAD identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub access_key: Option<ConfigValue<String>>,

    /// Wire schema (CloudEvents or EventGrid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<EventGridSchema>,

    /// Output format (packed/unpacked/template).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,

    /// Per-request timeout in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    /// Priority queue capacity for inbound query results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub priority_queue_capacity: Option<ConfigValue<u64>>,

    /// Per-query and default output templates (template format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<EventGridOutputTemplatesDto>,

    /// Permit plaintext `http` endpoints (local testing only). Defaults to false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_http: Option<bool>,
}

// ---------------------------------------------------------------------------
// DTO → domain conversion
// ---------------------------------------------------------------------------

fn map_template_spec(dto: &EventGridTemplateSpecDto) -> EventGridTemplateSpec {
    EventGridTemplateSpec {
        template: dto.template.clone(),
        extension: EventGridTemplateExt {
            metadata: dto.metadata.clone(),
        },
    }
}

fn map_query_config(dto: &EventGridQueryConfigDto) -> EventGridQueryConfig {
    EventGridQueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

fn map_output_templates(dto: &EventGridOutputTemplatesDto) -> EventGridOutputTemplates {
    EventGridOutputTemplates {
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

fn dto_template_spec(spec: &EventGridTemplateSpec) -> EventGridTemplateSpecDto {
    EventGridTemplateSpecDto {
        template: spec.template.clone(),
        metadata: spec.extension.metadata.clone(),
    }
}

fn dto_query_config(qc: &EventGridQueryConfig) -> EventGridQueryConfigDto {
    EventGridQueryConfigDto {
        added: qc.added.as_ref().map(dto_template_spec),
        updated: qc.updated.as_ref().map(dto_template_spec),
        deleted: qc.deleted.as_ref().map(dto_template_spec),
    }
}

fn dto_output_templates(t: &EventGridOutputTemplates) -> EventGridOutputTemplatesDto {
    EventGridOutputTemplatesDto {
        default_template: t.default_template.as_ref().map(dto_query_config),
        routes: t
            .routes
            .iter()
            .map(|(k, v)| (k.clone(), dto_query_config(v)))
            .collect(),
    }
}

/// Domain → DTO conversion used by `properties()` on the **embedded**
/// (non-descriptor) construction path. `priority_queue_capacity` is a
/// `ReactionBase` parameter rather than a field of the config, so it is emitted
/// as `None` here; the descriptor path preserves the raw config JSON instead.
impl From<&EventGridReactionConfig> for EventGridReactionConfigDto {
    fn from(c: &EventGridReactionConfig) -> Self {
        Self {
            endpoint: ConfigValue::Static(c.endpoint.clone()),
            access_key: c
                .access_key
                .as_ref()
                .map(|k| ConfigValue::Static(k.clone())),
            schema: Some(c.schema),
            format: Some(c.format),
            timeout_ms: Some(ConfigValue::Static(c.timeout_ms)),
            priority_queue_capacity: None,
            output_templates: c.output_templates.as_ref().map(dto_output_templates),
            allow_http: c.allow_http.then_some(true),
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAPI schemas
// ---------------------------------------------------------------------------

#[derive(OpenApi)]
#[openapi(components(schemas(
    EventGridReactionConfigDto,
    EventGridOutputTemplatesDto,
    EventGridQueryConfigDto,
    EventGridTemplateSpecDto,
    EventGridSchema,
    OutputFormat,
)))]
struct EventGridReactionSchemas;

/// Descriptor for the Event Grid reaction plugin.
pub struct EventGridReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for EventGridReactionDescriptor {
    fn kind(&self) -> &str {
        "eventgrid"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.eventgrid.EventGridReactionConfig"
    }

    fn display_name(&self) -> &str {
        "Azure Event Grid"
    }

    fn display_description(&self) -> &str {
        "Publishes Drasi query-result changes to an Azure Event Grid custom topic."
    }

    fn display_icon(&self) -> &str {
        "azure"
    }

    fn config_schema_json(&self) -> String {
        let api = EventGridReactionSchemas::openapi();
        api.components
            .as_ref()
            .and_then(|c| serde_json::to_string(&c.schemas).ok())
            .unwrap_or_else(|| "{}".to_string())
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: EventGridReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = EventGridReaction::builder(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint).await?);

        if let Some(access_key) = &dto.access_key {
            builder = builder.with_access_key(mapper.resolve_string(access_key).await?);
        }
        if let Some(schema) = dto.schema {
            builder = builder.with_schema(schema);
        }
        if let Some(format) = dto.format {
            builder = builder.with_format(format);
        }
        if let Some(timeout) = &dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(timeout).await?);
        }
        if let Some(capacity) = &dto.priority_queue_capacity {
            builder = builder
                .with_priority_queue_capacity(mapper.resolve_typed(capacity).await? as usize);
        }
        if let Some(templates) = &dto.output_templates {
            builder = builder.with_output_templates(map_output_templates(templates));
        }
        if let Some(allow_http) = dto.allow_http {
            builder = builder.with_allow_http(allow_http);
        }

        let mut reaction = builder.build()?;
        // Preserve the raw config JSON for lossless persistence round-trips
        // (ConfigValue secrets/env-vars and priorityQueueCapacity).
        reaction.base.set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}
