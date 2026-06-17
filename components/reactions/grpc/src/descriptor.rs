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

//! Descriptor for the gRPC reaction plugin.

use std::collections::HashMap;

use drasi_lib::reactions::common::{AdaptiveBatchConfig, QueryConfig, TemplateSpec};
use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{BatchingConfig, GrpcReactionConfig, OutputTemplates};
use crate::GrpcReactionBuilder;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::BatchingConfig)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum BatchingConfigDto {
    #[serde(rename = "fixed")]
    Fixed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueUsize>)]
        batch_size: Option<ConfigValue<usize>>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueU64>)]
        batch_flush_timeout_ms: Option<ConfigValue<u64>>,
    },
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueUsize>)]
        adaptive_min_batch_size: Option<ConfigValue<usize>>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueUsize>)]
        adaptive_max_batch_size: Option<ConfigValue<usize>>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueUsize>)]
        adaptive_window_size: Option<ConfigValue<usize>>,

        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueU64>)]
        adaptive_batch_timeout_ms: Option<ConfigValue<u64>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::TemplateSpec)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSpecDto {
    pub template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema, Default)]
#[schema(as = reaction::grpc::QueryConfig)]
#[serde(rename_all = "camelCase")]
pub struct QueryConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpecDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpecDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpecDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema, Default)]
#[schema(as = reaction::grpc::OutputTemplates)]
#[serde(rename_all = "camelCase")]
pub struct OutputTemplatesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfigDto>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfigDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::GrpcReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct GrpcReactionConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub endpoint: ConfigValue<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub timeout_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub max_retries: Option<ConfigValue<u32>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub connection_retry_attempts: Option<ConfigValue<u32>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub initial_connection_timeout_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batching: Option<BatchingConfigDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<OutputTemplatesDto>,
}

impl From<&GrpcReactionConfig> for GrpcReactionConfigDto {
    fn from(cfg: &GrpcReactionConfig) -> Self {
        Self {
            endpoint: ConfigValue::Static(cfg.endpoint.clone()),
            timeout_ms: Some(ConfigValue::Static(cfg.timeout_ms)),
            max_retries: Some(ConfigValue::Static(cfg.max_retries)),
            connection_retry_attempts: Some(ConfigValue::Static(cfg.connection_retry_attempts)),
            initial_connection_timeout_ms: Some(ConfigValue::Static(
                cfg.initial_connection_timeout_ms,
            )),
            metadata: cfg
                .metadata
                .iter()
                .map(|(k, v)| (k.clone(), ConfigValue::Static(v.clone())))
                .collect(),
            batching: Some(BatchingConfigDto::from(&cfg.batching)),
            output_templates: cfg.output_templates.as_ref().map(OutputTemplatesDto::from),
        }
    }
}

impl From<&BatchingConfig> for BatchingConfigDto {
    fn from(b: &BatchingConfig) -> Self {
        match b {
            BatchingConfig::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => Self::Fixed {
                batch_size: Some(ConfigValue::Static(*batch_size)),
                batch_flush_timeout_ms: Some(ConfigValue::Static(*batch_flush_timeout_ms)),
            },
            BatchingConfig::Adaptive {
                adaptive_min_batch_size,
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
            } => Self::Adaptive {
                adaptive_min_batch_size: Some(ConfigValue::Static(*adaptive_min_batch_size)),
                adaptive_max_batch_size: Some(ConfigValue::Static(*adaptive_max_batch_size)),
                adaptive_window_size: Some(ConfigValue::Static(*adaptive_window_size)),
                adaptive_batch_timeout_ms: Some(ConfigValue::Static(*adaptive_batch_timeout_ms)),
            },
        }
    }
}

impl From<&OutputTemplates> for OutputTemplatesDto {
    fn from(t: &OutputTemplates) -> Self {
        Self {
            default_template: t.default_template.as_ref().map(QueryConfigDto::from),
            routes: t
                .routes
                .iter()
                .map(|(k, v)| (k.clone(), QueryConfigDto::from(v)))
                .collect(),
        }
    }
}

impl From<&QueryConfig> for QueryConfigDto {
    fn from(q: &QueryConfig) -> Self {
        Self {
            added: q.added.as_ref().map(TemplateSpecDto::from),
            updated: q.updated.as_ref().map(TemplateSpecDto::from),
            deleted: q.deleted.as_ref().map(TemplateSpecDto::from),
        }
    }
}

impl From<&TemplateSpec> for TemplateSpecDto {
    fn from(s: &TemplateSpec) -> Self {
        Self {
            template: s.template.clone(),
        }
    }
}

impl From<&TemplateSpecDto> for TemplateSpec {
    fn from(s: &TemplateSpecDto) -> Self {
        TemplateSpec::new(s.template.clone())
    }
}

impl From<&QueryConfigDto> for QueryConfig {
    fn from(q: &QueryConfigDto) -> Self {
        QueryConfig {
            added: q.added.as_ref().map(TemplateSpec::from),
            updated: q.updated.as_ref().map(TemplateSpec::from),
            deleted: q.deleted.as_ref().map(TemplateSpec::from),
        }
    }
}

impl From<&OutputTemplatesDto> for OutputTemplates {
    fn from(t: &OutputTemplatesDto) -> Self {
        OutputTemplates {
            default_template: t.default_template.as_ref().map(QueryConfig::from),
            routes: t
                .routes
                .iter()
                .map(|(k, v)| (k.clone(), QueryConfig::from(v)))
                .collect(),
        }
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    GrpcReactionConfigDto,
    BatchingConfigDto,
    OutputTemplatesDto,
    QueryConfigDto,
    TemplateSpecDto,
)))]
struct GrpcReactionSchemas;

pub struct GrpcReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for GrpcReactionDescriptor {
    fn kind(&self) -> &str {
        "grpc"
    }

    fn config_version(&self) -> &str {
        "3.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.grpc.GrpcReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = GrpcReactionSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");

        SchemaUiAnnotator::new(schemas, "reaction.grpc.GrpcReactionConfig")
            .expect("root schema not found")
            .field("endpoint", |f| {
                f.group("Connection")
                    .order(1)
                    .placeholder("grpc://localhost:50052")
            })
            .field("timeoutMs", |f| {
                f.group("Connection").order(2).placeholder("5000")
            })
            .field("initialConnectionTimeoutMs", |f| {
                f.group("Connection").order(3).placeholder("10000")
            })
            .field("connectionRetryAttempts", |f| {
                f.group("Reliability").order(10).placeholder("5")
            })
            .field("maxRetries", |f| {
                f.group("Reliability").order(11).placeholder("3")
            })
            .field("metadata", |f| f.group("Auth").order(20))
            .field("batching", |f| f.group("Batching").order(30))
            .field("outputTemplates", |f| f.group("Templates").order(40))
            .annotate()
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: GrpcReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = GrpcReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_endpoint(mapper.resolve_string(&dto.endpoint).await?);

        if let Some(ref v) = dto.timeout_ms {
            builder = builder.with_timeout_ms(mapper.resolve_typed(v).await?);
        }
        if let Some(ref v) = dto.max_retries {
            builder = builder.with_max_retries(mapper.resolve_typed(v).await?);
        }
        if let Some(ref v) = dto.connection_retry_attempts {
            builder = builder.with_connection_retry_attempts(mapper.resolve_typed(v).await?);
        }
        if let Some(ref v) = dto.initial_connection_timeout_ms {
            builder = builder.with_initial_connection_timeout_ms(mapper.resolve_typed(v).await?);
        }
        for (key, value) in &dto.metadata {
            builder = builder.with_metadata(key, mapper.resolve_string(value).await?);
        }

        if let Some(ref batching) = dto.batching {
            let resolved = resolve_batching(&mapper, batching).await?;
            builder = builder.with_batching(resolved);
        }

        if let Some(ref templates) = dto.output_templates {
            builder = builder.with_output_templates(OutputTemplates::from(templates));
        }

        let mut reaction = builder.build()?;
        reaction.base_mut().set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}

async fn resolve_batching(
    mapper: &DtoMapper,
    dto: &BatchingConfigDto,
) -> anyhow::Result<BatchingConfig> {
    match dto {
        BatchingConfigDto::Fixed {
            batch_size,
            batch_flush_timeout_ms,
        } => {
            let size = if let Some(v) = batch_size {
                mapper.resolve_typed(v).await?
            } else {
                crate::config::default_batch_size()
            };
            let flush = if let Some(v) = batch_flush_timeout_ms {
                mapper.resolve_typed(v).await?
            } else {
                crate::config::default_batch_flush_timeout_ms()
            };
            Ok(BatchingConfig::Fixed {
                batch_size: size,
                batch_flush_timeout_ms: flush,
            })
        }
        BatchingConfigDto::Adaptive {
            adaptive_min_batch_size,
            adaptive_max_batch_size,
            adaptive_window_size,
            adaptive_batch_timeout_ms,
        } => {
            let defaults = AdaptiveBatchConfig::default();
            let min = if let Some(v) = adaptive_min_batch_size {
                mapper.resolve_typed(v).await?
            } else {
                defaults.adaptive_min_batch_size
            };
            let max = if let Some(v) = adaptive_max_batch_size {
                mapper.resolve_typed(v).await?
            } else {
                defaults.adaptive_max_batch_size
            };
            let win = if let Some(v) = adaptive_window_size {
                mapper.resolve_typed(v).await?
            } else {
                defaults.adaptive_window_size
            };
            let timeout = if let Some(v) = adaptive_batch_timeout_ms {
                mapper.resolve_typed(v).await?
            } else {
                defaults.adaptive_batch_timeout_ms
            };
            Ok(BatchingConfig::adaptive(AdaptiveBatchConfig {
                adaptive_min_batch_size: min,
                adaptive_max_batch_size: max,
                adaptive_window_size: win,
                adaptive_batch_timeout_ms: timeout,
            }))
        }
    }
}
