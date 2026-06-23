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

use crate::config::{
    BatchingConfig, GrpcReactionConfig, GrpcTemplateExtension, OutputFormat, OutputTemplates,
};
use crate::GrpcReactionBuilder;

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::BatchingConfig)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum BatchingConfigDto {
    #[serde(rename = "fixed")]
    Fixed {
        #[serde(default, rename = "batchSize", skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Option<ConfigValueUsize>)]
        batch_size: Option<ConfigValue<usize>>,

        #[serde(
            default,
            rename = "batchFlushTimeoutMs",
            skip_serializing_if = "Option::is_none"
        )]
        #[schema(value_type = Option<ConfigValueU64>)]
        batch_flush_timeout_ms: Option<ConfigValue<u64>>,
    },
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(
            default,
            rename = "adaptiveMinBatchSize",
            skip_serializing_if = "Option::is_none"
        )]
        #[schema(value_type = Option<ConfigValueUsize>)]
        adaptive_min_batch_size: Option<ConfigValue<usize>>,

        #[serde(
            default,
            rename = "adaptiveMaxBatchSize",
            skip_serializing_if = "Option::is_none"
        )]
        #[schema(value_type = Option<ConfigValueUsize>)]
        adaptive_max_batch_size: Option<ConfigValue<usize>>,

        #[serde(
            default,
            rename = "adaptiveWindowSize",
            skip_serializing_if = "Option::is_none"
        )]
        #[schema(value_type = Option<ConfigValueUsize>)]
        adaptive_window_size: Option<ConfigValue<usize>>,

        #[serde(
            default,
            rename = "adaptiveBatchTimeoutMs",
            skip_serializing_if = "Option::is_none"
        )]
        #[schema(value_type = Option<ConfigValueU64>)]
        adaptive_batch_timeout_ms: Option<ConfigValue<u64>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::TemplateSpec)]
#[serde(rename_all = "camelCase")]
pub struct TemplateSpecDto {
    pub template: String,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
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
    pub output_format: Option<OutputFormat>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batching: Option<BatchingConfigDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<OutputTemplatesDto>,

    /// Recovery policy applied on a sustained delivery failure or checkpoint
    /// gap. When omitted, the reaction's archetype default (`strict`) applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_policy: Option<RecoveryPolicyDto>,
}

/// Recovery policy as exposed in declarative (JSON/YAML) config.
///
/// Only the two variants valid for this reaction are exposed; `AutoReset` is
/// omitted because it requires a fresh-start snapshot, which a trigger reaction
/// (`needs_snapshot_on_fresh_start = false`) does not have — the host rejects
/// that combination at startup, so the schema disallows it up front.
///
/// * `strict` (default) — fail-stop on sustained delivery failure; the un-acked
///   batch replays from the query outbox on restart.
/// * `auto_skip_gap` — drop the failed batch and continue (favor uptime).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::grpc::RecoveryPolicy)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicyDto {
    Strict,
    AutoSkipGap,
}

impl From<RecoveryPolicyDto> for drasi_lib::recovery::ReactionRecoveryPolicy {
    fn from(dto: RecoveryPolicyDto) -> Self {
        match dto {
            RecoveryPolicyDto::Strict => Self::Strict,
            RecoveryPolicyDto::AutoSkipGap => Self::AutoSkipGap,
        }
    }
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
            output_format: Some(cfg.output_format),
            batching: Some(BatchingConfigDto::from(&cfg.batching)),
            output_templates: cfg.output_templates.as_ref().map(OutputTemplatesDto::from),
            // `recovery_policy` is a `ReactionBase` parameter, not a field of
            // `GrpcReactionConfig`; lossless on the descriptor path via raw config.
            recovery_policy: None,
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

impl From<&QueryConfig<GrpcTemplateExtension>> for QueryConfigDto {
    fn from(q: &QueryConfig<GrpcTemplateExtension>) -> Self {
        Self {
            added: q.added.as_ref().map(TemplateSpecDto::from),
            updated: q.updated.as_ref().map(TemplateSpecDto::from),
            deleted: q.deleted.as_ref().map(TemplateSpecDto::from),
        }
    }
}

impl From<&TemplateSpec<GrpcTemplateExtension>> for TemplateSpecDto {
    fn from(s: &TemplateSpec<GrpcTemplateExtension>) -> Self {
        Self {
            template: s.template.clone(),
            metadata: s.extension.metadata.clone(),
        }
    }
}

impl From<&TemplateSpecDto> for TemplateSpec<GrpcTemplateExtension> {
    fn from(s: &TemplateSpecDto) -> Self {
        TemplateSpec::with_extension(
            s.template.clone(),
            GrpcTemplateExtension {
                metadata: s.metadata.clone(),
            },
        )
    }
}

impl From<&QueryConfigDto> for QueryConfig<GrpcTemplateExtension> {
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
    OutputFormat,
    BatchingConfigDto,
    OutputTemplatesDto,
    QueryConfigDto,
    TemplateSpecDto,
    RecoveryPolicyDto,
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

    fn display_name(&self) -> &str {
        "gRPC"
    }

    fn display_description(&self) -> &str {
        "Forwards continuous query result changes to a downstream gRPC service via \
         the ReactionService.ProcessResults RPC, with fixed or adaptive batching and \
         optional Handlebars output templates."
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
            .field("outputFormat", |f| {
                f.group("Output").order(35).placeholder("canonicalJson")
            })
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

        if let Some(output_format) = dto.output_format {
            builder = builder.with_output_format(output_format);
        }

        if let Some(ref batching) = dto.batching {
            let resolved = resolve_batching(&mapper, batching).await?;
            builder = builder.with_batching(resolved);
        }

        if let Some(ref templates) = dto.output_templates {
            builder = builder.with_output_templates(OutputTemplates::from(templates));
        }

        if let Some(policy) = dto.recovery_policy {
            builder = builder.with_recovery_policy(policy.into());
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;

    // ---- resolve_batching ------------------------------------------------

    #[tokio::test]
    async fn resolve_batching_fixed_uses_provided_values() {
        let mapper = DtoMapper::new();
        let dto = BatchingConfigDto::Fixed {
            batch_size: Some(ConfigValue::Static(7)),
            batch_flush_timeout_ms: Some(ConfigValue::Static(8)),
        };
        let resolved = resolve_batching(&mapper, &dto).await.unwrap();
        assert_eq!(
            resolved,
            BatchingConfig::Fixed {
                batch_size: 7,
                batch_flush_timeout_ms: 8
            }
        );
    }

    #[tokio::test]
    async fn resolve_batching_fixed_fills_missing_fields_with_defaults() {
        let mapper = DtoMapper::new();
        let dto = BatchingConfigDto::Fixed {
            batch_size: None,
            batch_flush_timeout_ms: None,
        };
        let resolved = resolve_batching(&mapper, &dto).await.unwrap();
        assert_eq!(
            resolved,
            BatchingConfig::Fixed {
                batch_size: crate::config::default_batch_size(),
                batch_flush_timeout_ms: crate::config::default_batch_flush_timeout_ms(),
            }
        );
    }

    #[tokio::test]
    async fn resolve_batching_adaptive_uses_provided_values() {
        let mapper = DtoMapper::new();
        let dto = BatchingConfigDto::Adaptive {
            adaptive_min_batch_size: Some(ConfigValue::Static(2)),
            adaptive_max_batch_size: Some(ConfigValue::Static(20)),
            adaptive_window_size: Some(ConfigValue::Static(3)),
            adaptive_batch_timeout_ms: Some(ConfigValue::Static(30)),
        };
        let resolved = resolve_batching(&mapper, &dto).await.unwrap();
        assert_eq!(
            resolved,
            BatchingConfig::Adaptive {
                adaptive_min_batch_size: 2,
                adaptive_max_batch_size: 20,
                adaptive_window_size: 3,
                adaptive_batch_timeout_ms: 30,
            }
        );
    }

    #[tokio::test]
    async fn resolve_batching_adaptive_fills_missing_fields_with_defaults() {
        let mapper = DtoMapper::new();
        let dto = BatchingConfigDto::Adaptive {
            adaptive_min_batch_size: None,
            adaptive_max_batch_size: None,
            adaptive_window_size: None,
            adaptive_batch_timeout_ms: None,
        };
        let resolved = resolve_batching(&mapper, &dto).await.unwrap();
        assert_eq!(
            resolved,
            BatchingConfig::adaptive(AdaptiveBatchConfig::default())
        );
    }

    // ---- DTO conversions -------------------------------------------------

    #[test]
    fn dto_from_config_preserves_scalar_fields() {
        let mut metadata = HashMap::new();
        metadata.insert("k".to_string(), "v".to_string());
        let cfg = GrpcReactionConfig {
            endpoint: "grpc://h:1".to_string(),
            timeout_ms: 11,
            max_retries: 2,
            connection_retry_attempts: 3,
            initial_connection_timeout_ms: 44,
            metadata,
            batching: BatchingConfig::Fixed {
                batch_size: 5,
                batch_flush_timeout_ms: 6,
            },
            output_format: OutputFormat::Proto,
            output_templates: None,
        };
        let json = serde_json::to_value(GrpcReactionConfigDto::from(&cfg)).unwrap();
        assert_eq!(json["endpoint"], serde_json::json!("grpc://h:1"));
        assert_eq!(json["timeoutMs"], serde_json::json!(11));
        assert_eq!(json["maxRetries"], serde_json::json!(2));
        assert_eq!(json["connectionRetryAttempts"], serde_json::json!(3));
        assert_eq!(json["initialConnectionTimeoutMs"], serde_json::json!(44));
        assert_eq!(json["outputFormat"], serde_json::json!("proto"));
        assert_eq!(json["batching"]["mode"], serde_json::json!("fixed"));
        assert_eq!(json["batching"]["batchSize"], serde_json::json!(5));
        assert_eq!(json["metadata"]["k"], serde_json::json!("v"));
    }

    #[test]
    fn dto_from_adaptive_batching_preserves_fields() {
        let dto = BatchingConfigDto::from(&BatchingConfig::Adaptive {
            adaptive_min_batch_size: 4,
            adaptive_max_batch_size: 40,
            adaptive_window_size: 5,
            adaptive_batch_timeout_ms: 50,
        });
        let json = serde_json::to_value(dto).unwrap();
        assert_eq!(json["mode"], serde_json::json!("adaptive"));
        assert_eq!(json["adaptiveMinBatchSize"], serde_json::json!(4));
        assert_eq!(json["adaptiveMaxBatchSize"], serde_json::json!(40));
        assert_eq!(json["adaptiveWindowSize"], serde_json::json!(5));
        assert_eq!(json["adaptiveBatchTimeoutMs"], serde_json::json!(50));
    }

    #[test]
    fn batching_dto_deserializes_camel_case_keys() {
        // Regression: batching fields must accept the camelCase keys used by the
        // runtime BatchingConfig, the generated schema, and the docs. Before the
        // per-field renames, `rename_all` on the enum did not reach struct-variant
        // fields, so camelCase input was silently dropped to defaults.
        let dto: BatchingConfigDto = serde_json::from_value(serde_json::json!({
            "mode": "fixed", "batchSize": 50, "batchFlushTimeoutMs": 250
        }))
        .unwrap();
        match dto {
            BatchingConfigDto::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => {
                assert!(
                    batch_size.is_some(),
                    "batchSize must populate the DTO field"
                );
                assert!(batch_flush_timeout_ms.is_some());
            }
            _ => panic!("expected fixed"),
        }

        let dto: BatchingConfigDto = serde_json::from_value(serde_json::json!({
            "mode": "adaptive",
            "adaptiveMinBatchSize": 2,
            "adaptiveMaxBatchSize": 20,
            "adaptiveWindowSize": 3,
            "adaptiveBatchTimeoutMs": 30
        }))
        .unwrap();
        match dto {
            BatchingConfigDto::Adaptive {
                adaptive_min_batch_size,
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
            } => {
                assert!(adaptive_min_batch_size.is_some());
                assert!(adaptive_max_batch_size.is_some());
                assert!(adaptive_window_size.is_some());
                assert!(adaptive_batch_timeout_ms.is_some());
            }
            _ => panic!("expected adaptive"),
        }
    }

    #[test]
    fn descriptor_schema_uses_camel_case_batching_keys() {
        let schema = GrpcReactionDescriptor.config_schema_json();
        for key in [
            "batchSize",
            "batchFlushTimeoutMs",
            "adaptiveMinBatchSize",
            "adaptiveMaxBatchSize",
            "adaptiveWindowSize",
            "adaptiveBatchTimeoutMs",
        ] {
            assert!(
                schema.contains(key),
                "schema must advertise camelCase batching key {key}"
            );
        }
        assert!(
            !schema.contains("batch_size"),
            "schema must not leak snake_case batching keys"
        );
    }

    #[test]
    fn template_spec_dto_round_trip_preserves_metadata() {
        let mut metadata = HashMap::new();
        metadata.insert("x".to_string(), "{{operation}}".to_string());
        let spec = TemplateSpec::with_extension(
            r#"{"id":1}"#,
            GrpcTemplateExtension {
                metadata: metadata.clone(),
            },
        );
        let dto = TemplateSpecDto::from(&spec);
        assert_eq!(dto.template, r#"{"id":1}"#);
        assert_eq!(dto.metadata, metadata);

        let back = TemplateSpec::<GrpcTemplateExtension>::from(&dto);
        assert_eq!(back.template, spec.template);
        assert_eq!(back.extension.metadata, metadata);
    }

    #[test]
    fn output_templates_dto_round_trips_through_domain_type() {
        let mut routes = HashMap::new();
        routes.insert(
            "q1".to_string(),
            QueryConfig {
                added: Some(TemplateSpec::with_extension(
                    r#"{"a":1}"#,
                    GrpcTemplateExtension::default(),
                )),
                updated: None,
                deleted: None,
            },
        );
        let templates = OutputTemplates {
            default_template: Some(QueryConfig {
                added: None,
                updated: Some(TemplateSpec::with_extension(
                    r#"{"u":1}"#,
                    GrpcTemplateExtension::default(),
                )),
                deleted: None,
            }),
            routes,
        };
        let dto = OutputTemplatesDto::from(&templates);
        let back = OutputTemplates::from(&dto);
        assert_eq!(back, templates);
    }

    // ---- public descriptor surface --------------------------------------

    #[test]
    fn descriptor_metadata_surface_is_stable() {
        let d = GrpcReactionDescriptor;
        assert_eq!(d.kind(), "grpc");
        assert_eq!(d.config_version(), "3.0.0");
        assert_eq!(d.display_name(), "gRPC");
        assert!(d.display_description().to_lowercase().contains("grpc"));
        assert_eq!(d.config_schema_name(), "reaction.grpc.GrpcReactionConfig");
    }

    #[test]
    fn descriptor_schema_json_carries_ui_group_annotations() {
        let schema = GrpcReactionDescriptor.config_schema_json();
        for group in [
            "Connection",
            "Reliability",
            "Auth",
            "Batching",
            "Output",
            "Templates",
        ] {
            assert!(schema.contains(group), "schema missing UI group {group}");
        }
    }

    // ---- create_reaction -------------------------------------------------

    #[tokio::test]
    async fn create_reaction_resolves_adaptive_batching() {
        let cfg = serde_json::json!({
            "endpoint": "grpc://h:1",
            "batching": { "mode": "adaptive", "adaptiveMinBatchSize": 2, "adaptiveMaxBatchSize": 20 }
        });
        let reaction = GrpcReactionDescriptor
            .create_reaction("id", vec![], &cfg, true)
            .await
            .expect("adaptive create_reaction succeeds");
        // raw_config drives properties(), so the input batching mode is echoed
        // back — confirming the adaptive config was accepted and validated.
        let props = reaction.properties();
        assert_eq!(props["batching"]["mode"], serde_json::json!("adaptive"));
    }

    #[tokio::test]
    async fn create_reaction_minimal_config_uses_defaults_and_auto_start_flag() {
        let cfg = serde_json::json!({ "endpoint": "grpc://h:1" });
        let reaction = GrpcReactionDescriptor
            .create_reaction("id", vec![], &cfg, false)
            .await
            .expect("minimal create_reaction succeeds");
        assert_eq!(reaction.id(), "id");
        assert!(!reaction.auto_start(), "auto_start flag must be honored");
        assert_eq!(reaction.type_name(), "grpc");
    }

    #[tokio::test]
    async fn create_reaction_rejects_invalid_template() {
        let cfg = serde_json::json!({
            "endpoint": "grpc://h:1",
            "outputTemplates": { "defaultTemplate": { "added": { "template": "{{" } } }
        });
        let result = GrpcReactionDescriptor
            .create_reaction("id", vec!["q1".into()], &cfg, true)
            .await;
        assert!(
            result.is_err(),
            "an invalid template must fail create_reaction"
        );
    }

    #[tokio::test]
    async fn create_reaction_rejects_route_key_not_matching_a_query() {
        let cfg = serde_json::json!({
            "endpoint": "grpc://h:1",
            "outputTemplates": { "routes": { "ghost": { "added": { "template": "{\"id\":1}" } } } }
        });
        let result = GrpcReactionDescriptor
            .create_reaction("id", vec!["q1".into()], &cfg, true)
            .await;
        assert!(
            result.is_err(),
            "a route key matching no subscribed query must fail create_reaction"
        );
    }

    #[test]
    fn dto_parses_recovery_policy_snake_case_into_reaction_policy() {
        use drasi_lib::recovery::ReactionRecoveryPolicy;
        let dto: GrpcReactionConfigDto = serde_json::from_value(serde_json::json!({
            "endpoint": "grpc://h:1",
            "recoveryPolicy": "auto_skip_gap"
        }))
        .expect("recoveryPolicy parses");
        assert_eq!(dto.recovery_policy, Some(RecoveryPolicyDto::AutoSkipGap));
        assert_eq!(
            ReactionRecoveryPolicy::from(dto.recovery_policy.unwrap()),
            ReactionRecoveryPolicy::AutoSkipGap
        );

        // Omitted → None (archetype default applies).
        let dto: GrpcReactionConfigDto =
            serde_json::from_value(serde_json::json!({ "endpoint": "grpc://h:1" })).unwrap();
        assert_eq!(dto.recovery_policy, None);

        // `auto_reset` is not valid for this (non-snapshot) reaction, so the
        // schema rejects it up front rather than deferring to a startup failure.
        assert!(serde_json::from_value::<GrpcReactionConfigDto>(serde_json::json!({
            "endpoint": "grpc://h:1",
            "recoveryPolicy": "auto_reset"
        }))
        .is_err());
    }

    #[tokio::test]
    async fn create_reaction_accepts_declarative_recovery_policy() {
        let cfg = serde_json::json!({
            "endpoint": "grpc://h:1",
            "recoveryPolicy": "auto_skip_gap"
        });
        let reaction = GrpcReactionDescriptor
            .create_reaction("id", vec![], &cfg, true)
            .await
            .expect("recoveryPolicy create_reaction succeeds");
        // raw_config drives properties(), so the input policy is echoed back —
        // confirming the field was accepted (deny-unknown would otherwise reject).
        assert_eq!(
            reaction.properties()["recoveryPolicy"],
            serde_json::json!("auto_skip_gap")
        );
    }
}
