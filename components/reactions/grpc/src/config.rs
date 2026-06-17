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

//! Configuration types for the gRPC reaction.

use drasi_lib::reactions::common::{AdaptiveBatchConfig, QueryConfig, TemplateRouting};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub(crate) fn default_grpc_endpoint() -> String {
    "grpc://localhost:50052".to_string()
}

pub(crate) fn default_timeout_ms() -> u64 {
    5000
}

pub(crate) fn default_batch_size() -> usize {
    100
}

pub(crate) fn default_batch_flush_timeout_ms() -> u64 {
    1000
}

pub(crate) fn default_max_retries() -> u32 {
    3
}

pub(crate) fn default_connection_retry_attempts() -> u32 {
    5
}

pub(crate) fn default_initial_connection_timeout_ms() -> u64 {
    10000
}

pub(crate) fn default_adaptive_min_batch_size() -> usize {
    1
}

pub(crate) fn default_adaptive_max_batch_size() -> usize {
    100
}

pub(crate) fn default_adaptive_window_size() -> usize {
    10
}

pub(crate) fn default_adaptive_batch_timeout_ms() -> u64 {
    1000
}

/// Batching strategy used by the gRPC reaction.
///
/// All keys serialize and deserialize in `camelCase` to stay consistent with the
/// rest of the gRPC reaction configuration and its generated descriptor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum BatchingConfig {
    /// Fixed-size batching — flushes when `batchSize` items have accumulated
    /// or `batchFlushTimeoutMs` elapses, whichever comes first.
    #[serde(rename = "fixed")]
    Fixed {
        #[serde(default = "default_batch_size", rename = "batchSize")]
        batch_size: usize,
        #[serde(
            default = "default_batch_flush_timeout_ms",
            rename = "batchFlushTimeoutMs"
        )]
        batch_flush_timeout_ms: u64,
    },
    /// Throughput-aware adaptive batching.
    #[serde(rename = "adaptive")]
    Adaptive {
        #[serde(
            default = "default_adaptive_min_batch_size",
            rename = "adaptiveMinBatchSize"
        )]
        adaptive_min_batch_size: usize,
        #[serde(
            default = "default_adaptive_max_batch_size",
            rename = "adaptiveMaxBatchSize"
        )]
        adaptive_max_batch_size: usize,
        #[serde(
            default = "default_adaptive_window_size",
            rename = "adaptiveWindowSize"
        )]
        adaptive_window_size: usize,
        #[serde(
            default = "default_adaptive_batch_timeout_ms",
            rename = "adaptiveBatchTimeoutMs"
        )]
        adaptive_batch_timeout_ms: u64,
    },
}

impl Default for BatchingConfig {
    fn default() -> Self {
        BatchingConfig::Fixed {
            batch_size: default_batch_size(),
            batch_flush_timeout_ms: default_batch_flush_timeout_ms(),
        }
    }
}

impl BatchingConfig {
    /// Builds the adaptive batching variant from a shared [`AdaptiveBatchConfig`].
    pub(crate) fn adaptive(cfg: AdaptiveBatchConfig) -> Self {
        BatchingConfig::Adaptive {
            adaptive_min_batch_size: cfg.adaptive_min_batch_size,
            adaptive_max_batch_size: cfg.adaptive_max_batch_size,
            adaptive_window_size: cfg.adaptive_window_size,
            adaptive_batch_timeout_ms: cfg.adaptive_batch_timeout_ms,
        }
    }

    /// Returns the shared [`AdaptiveBatchConfig`] when this is the adaptive variant.
    pub(crate) fn as_adaptive_config(&self) -> Option<AdaptiveBatchConfig> {
        match *self {
            BatchingConfig::Adaptive {
                adaptive_min_batch_size,
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
            } => Some(AdaptiveBatchConfig {
                adaptive_min_batch_size,
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
            }),
            BatchingConfig::Fixed { .. } => None,
        }
    }
}

/// Output payload mode for the gRPC reaction.
///
/// `canonicalJson` keeps a guide-aligned canonical payload in every
/// `QueryResultItem.payload` while the protobuf fields continue to expose the
/// typed gRPC contract. `proto` omits that payload unless a body template is
/// configured for an item.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default, utoipa::ToSchema)]
#[schema(as = reaction::grpc::OutputFormat)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    #[default]
    CanonicalJson,
    Proto,
}

/// Per-template gRPC extension fields.
///
/// Metadata entries are rendered through Handlebars with the same standard
/// context as the body template, then merged with top-level request metadata
/// for the outbound RPC. This lets headers vary per query/operation/row while
/// preserving the fixed gRPC service endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GrpcTemplateExtension {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

pub type GrpcQueryConfig = QueryConfig<GrpcTemplateExtension>;

/// Optional Handlebars-based output template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutputTemplates {
    /// Default template applied when no per-query override matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<GrpcQueryConfig>,

    /// Per-query overrides keyed by query id.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, GrpcQueryConfig>,
}

impl OutputTemplates {
    /// Returns `true` if at least one non-empty template string is configured
    /// (either in the default template or any per-query route). Used to avoid
    /// allocating a Handlebars engine when no template would ever render.
    pub(crate) fn has_renderable_templates(&self) -> bool {
        fn has_nonempty(qc: &GrpcQueryConfig) -> bool {
            [qc.added.as_ref(), qc.updated.as_ref(), qc.deleted.as_ref()]
                .into_iter()
                .flatten()
                .any(|spec| !spec.template.trim().is_empty() || !spec.extension.metadata.is_empty())
        }
        self.default_template.as_ref().is_some_and(has_nonempty)
            || self.routes.values().any(has_nonempty)
    }

    /// Compile every configured template and check that every route key
    /// matches a subscribed query id (or its last dotted segment). Called
    /// at construction time so misconfiguration fails fast.
    pub(crate) fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        use anyhow::Context as _;
        if let Some(qc) = self.default_template.as_ref() {
            validate_query_config(qc).context("invalid defaultTemplate")?;
        }
        for (key, qc) in &self.routes {
            validate_query_config(qc)
                .with_context(|| format!("invalid outputTemplates route '{key}'"))?;
            if !route_key_matches(key, query_ids) {
                anyhow::bail!(
                    "outputTemplates route key '{key}' does not match any subscribed query id \
                     (or its last dotted segment)"
                );
            }
        }
        Ok(())
    }
}

/// Compile-check every template in a `QueryConfig` (added / updated / deleted).
fn validate_query_config(qc: &GrpcQueryConfig) -> anyhow::Result<()> {
    for spec in [qc.added.as_ref(), qc.updated.as_ref(), qc.deleted.as_ref()]
        .into_iter()
        .flatten()
    {
        crate::templates::validate_template(&spec.template)?;
        for (key, value_template) in &spec.extension.metadata {
            if key.trim().is_empty() {
                anyhow::bail!("metadata template key must not be empty");
            }
            crate::templates::validate_template(value_template)?;
        }
    }
    Ok(())
}

/// A route key is valid if it matches a subscribed query id exactly or
/// matches the last dotted segment of one (the `source.my_query` → `my_query`
/// fallback used during resolution).
fn route_key_matches(key: &str, query_ids: &[String]) -> bool {
    query_ids
        .iter()
        .any(|q| q == key || q.rsplit('.').next() == Some(key))
}

/// gRPC reaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GrpcReactionConfig {
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    #[serde(default = "default_connection_retry_attempts")]
    pub connection_retry_attempts: u32,

    #[serde(default = "default_initial_connection_timeout_ms")]
    pub initial_connection_timeout_ms: u64,

    #[serde(default)]
    pub metadata: HashMap<String, String>,

    #[serde(default)]
    pub batching: BatchingConfig,

    #[serde(default)]
    pub output_format: OutputFormat,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<OutputTemplates>,
}

impl Default for GrpcReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            timeout_ms: default_timeout_ms(),
            max_retries: default_max_retries(),
            connection_retry_attempts: default_connection_retry_attempts(),
            initial_connection_timeout_ms: default_initial_connection_timeout_ms(),
            metadata: HashMap::new(),
            batching: BatchingConfig::default(),
            output_format: OutputFormat::default(),
            output_templates: None,
        }
    }
}

impl GrpcReactionConfig {
    /// Validate the configuration at construction time. Compiles every
    /// output template and checks that every route key matches a subscribed
    /// query id (or its last dotted segment), so a misconfiguration fails at
    /// `build()` rather than per-event at dispatch.
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        validate_endpoint(&self.endpoint)?;
        validate_positive(self.timeout_ms, "timeoutMs")?;
        validate_positive(
            self.initial_connection_timeout_ms,
            "initialConnectionTimeoutMs",
        )?;
        match self.batching {
            BatchingConfig::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => {
                validate_positive(batch_size, "batching.batchSize")?;
                validate_positive(batch_flush_timeout_ms, "batching.batchFlushTimeoutMs")?;
            }
            BatchingConfig::Adaptive {
                adaptive_min_batch_size,
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
            } => {
                validate_positive(adaptive_min_batch_size, "batching.adaptiveMinBatchSize")?;
                validate_positive(adaptive_max_batch_size, "batching.adaptiveMaxBatchSize")?;
                validate_positive(adaptive_window_size, "batching.adaptiveWindowSize")?;
                validate_positive(adaptive_batch_timeout_ms, "batching.adaptiveBatchTimeoutMs")?;
                if adaptive_min_batch_size > adaptive_max_batch_size {
                    anyhow::bail!(
                        "batching.adaptiveMinBatchSize ({adaptive_min_batch_size}) must be <= \
                         batching.adaptiveMaxBatchSize ({adaptive_max_batch_size})"
                    );
                }
            }
        }
        if let Some(templates) = self.output_templates.as_ref() {
            templates.validate(query_ids)?;
        }
        Ok(())
    }
}

fn validate_endpoint(endpoint: &str) -> anyhow::Result<()> {
    let http_endpoint = endpoint.replace("grpc://", "http://");
    tonic::transport::Endpoint::from_shared(http_endpoint)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("invalid endpoint '{endpoint}': {e}"))
}

fn validate_positive<T>(value: T, field: &str) -> anyhow::Result<()>
where
    T: PartialEq + From<u8> + std::fmt::Display,
{
    if value == T::from(0) {
        anyhow::bail!("{field} must be greater than 0");
    }
    Ok(())
}

fn empty_routes() -> &'static HashMap<String, GrpcQueryConfig> {
    static EMPTY: std::sync::OnceLock<HashMap<String, GrpcQueryConfig>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

impl TemplateRouting<GrpcTemplateExtension> for GrpcReactionConfig {
    fn routes(&self) -> &HashMap<String, GrpcQueryConfig> {
        match self.output_templates.as_ref() {
            Some(t) => &t.routes,
            None => empty_routes(),
        }
    }

    fn default_template(&self) -> Option<&GrpcQueryConfig> {
        self.output_templates
            .as_ref()
            .and_then(|t| t.default_template.as_ref())
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use drasi_lib::reactions::common::TemplateSpec;

    fn with_templates(
        default: Option<GrpcQueryConfig>,
        routes: HashMap<String, GrpcQueryConfig>,
    ) -> GrpcReactionConfig {
        GrpcReactionConfig {
            output_templates: Some(OutputTemplates {
                default_template: default,
                routes,
            }),
            ..Default::default()
        }
    }

    fn qc_added(template: &str) -> GrpcQueryConfig {
        QueryConfig {
            added: Some(TemplateSpec::with_extension(
                template,
                GrpcTemplateExtension::default(),
            )),
            updated: None,
            deleted: None,
        }
    }

    #[test]
    fn validate_is_noop_without_templates() {
        GrpcReactionConfig::default().validate(&[]).unwrap();
    }

    #[test]
    fn validate_accepts_valid_default_template() {
        let cfg = with_templates(Some(qc_added(r#"{"id":"{{after.id}}"}"#)), HashMap::new());
        cfg.validate(&["q1".to_string()]).unwrap();
    }

    #[test]
    fn validate_rejects_invalid_template_syntax() {
        let cfg = with_templates(Some(qc_added("{{")), HashMap::new());
        let err = cfg.validate(&["q1".to_string()]).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("defaultTemplate") && msg.contains("template"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn validate_accepts_route_key_matching_query_id() {
        let mut routes = HashMap::new();
        routes.insert("q1".to_string(), qc_added(r#"{"id":"{{after.id}}"}"#));
        with_templates(None, routes)
            .validate(&["q1".to_string()])
            .unwrap();
    }

    #[test]
    fn validate_accepts_route_key_matching_last_dotted_segment() {
        let mut routes = HashMap::new();
        routes.insert("orders".to_string(), qc_added(r#"{"id":"{{after.id}}"}"#));
        with_templates(None, routes)
            .validate(&["source.orders".to_string()])
            .unwrap();
    }

    #[test]
    fn validate_rejects_route_key_not_matching_any_query() {
        let mut routes = HashMap::new();
        routes.insert("ghost".to_string(), qc_added(r#"{"id":"{{after.id}}"}"#));
        let err = with_templates(None, routes)
            .validate(&["q1".to_string()])
            .unwrap_err();
        assert!(err.to_string().contains("ghost"), "unexpected error: {err}");
    }

    #[test]
    fn validate_rejects_invalid_route_template() {
        let mut routes = HashMap::new();
        routes.insert("q1".to_string(), qc_added("{{#if}}"));
        let err = with_templates(None, routes)
            .validate(&["q1".to_string()])
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("q1") && msg.contains("template"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn validate_rejects_invalid_endpoint() {
        let cfg = GrpcReactionConfig {
            endpoint: "not a uri".to_string(),
            ..Default::default()
        };
        let err = cfg.validate(&[]).unwrap_err();
        assert!(err.to_string().contains("invalid endpoint"));
    }

    #[test]
    fn validate_rejects_zero_fixed_batch_size() {
        let cfg = GrpcReactionConfig {
            batching: BatchingConfig::Fixed {
                batch_size: 0,
                batch_flush_timeout_ms: 1000,
            },
            ..Default::default()
        };
        let err = cfg.validate(&[]).unwrap_err();
        assert!(err.to_string().contains("batchSize"));
    }

    #[test]
    fn validate_rejects_inverted_adaptive_bounds() {
        let cfg = GrpcReactionConfig {
            batching: BatchingConfig::Adaptive {
                adaptive_min_batch_size: 10,
                adaptive_max_batch_size: 1,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 1000,
            },
            ..Default::default()
        };
        let err = cfg.validate(&[]).unwrap_err();
        assert!(err.to_string().contains("adaptiveMinBatchSize"));
    }

    #[test]
    fn validate_rejects_zero_timeout_ms() {
        let cfg = GrpcReactionConfig {
            timeout_ms: 0,
            ..Default::default()
        };
        let err = cfg.validate(&[]).unwrap_err();
        assert!(err.to_string().contains("timeoutMs"), "got: {err}");
    }

    #[test]
    fn validate_rejects_zero_initial_connection_timeout_ms() {
        let cfg = GrpcReactionConfig {
            initial_connection_timeout_ms: 0,
            ..Default::default()
        };
        let err = cfg.validate(&[]).unwrap_err();
        assert!(
            err.to_string().contains("initialConnectionTimeoutMs"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_rejects_zero_fixed_flush_timeout() {
        let cfg = GrpcReactionConfig {
            batching: BatchingConfig::Fixed {
                batch_size: 10,
                batch_flush_timeout_ms: 0,
            },
            ..Default::default()
        };
        let err = cfg.validate(&[]).unwrap_err();
        assert!(
            err.to_string().contains("batchFlushTimeoutMs"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_rejects_zero_adaptive_fields() {
        for (cfg, needle) in [
            (
                BatchingConfig::Adaptive {
                    adaptive_min_batch_size: 0,
                    adaptive_max_batch_size: 10,
                    adaptive_window_size: 10,
                    adaptive_batch_timeout_ms: 100,
                },
                "adaptiveMinBatchSize",
            ),
            (
                BatchingConfig::Adaptive {
                    adaptive_min_batch_size: 1,
                    adaptive_max_batch_size: 0,
                    adaptive_window_size: 10,
                    adaptive_batch_timeout_ms: 100,
                },
                "adaptiveMaxBatchSize",
            ),
            (
                BatchingConfig::Adaptive {
                    adaptive_min_batch_size: 1,
                    adaptive_max_batch_size: 10,
                    adaptive_window_size: 0,
                    adaptive_batch_timeout_ms: 100,
                },
                "adaptiveWindowSize",
            ),
            (
                BatchingConfig::Adaptive {
                    adaptive_min_batch_size: 1,
                    adaptive_max_batch_size: 10,
                    adaptive_window_size: 10,
                    adaptive_batch_timeout_ms: 0,
                },
                "adaptiveBatchTimeoutMs",
            ),
        ] {
            let reaction = GrpcReactionConfig {
                batching: cfg,
                ..Default::default()
            };
            let err = reaction.validate(&[]).unwrap_err();
            assert!(err.to_string().contains(needle), "got: {err}");
        }
    }

    #[test]
    fn validate_accepts_equal_adaptive_bounds() {
        let cfg = GrpcReactionConfig {
            batching: BatchingConfig::Adaptive {
                adaptive_min_batch_size: 7,
                adaptive_max_batch_size: 7,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 100,
            },
            ..Default::default()
        };
        cfg.validate(&[]).expect("min == max is allowed");
    }
}

#[cfg(test)]
mod behavior_tests {
    use super::*;
    use drasi_lib::reactions::common::TemplateSpec;

    fn qc(template: &str, metadata: HashMap<String, String>) -> GrpcQueryConfig {
        QueryConfig {
            added: Some(TemplateSpec::with_extension(
                template,
                GrpcTemplateExtension { metadata },
            )),
            updated: None,
            deleted: None,
        }
    }

    #[test]
    fn default_batching_is_fixed_with_documented_defaults() {
        match BatchingConfig::default() {
            BatchingConfig::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => {
                assert_eq!(batch_size, default_batch_size());
                assert_eq!(batch_flush_timeout_ms, default_batch_flush_timeout_ms());
            }
            _ => panic!("default batching must be fixed"),
        }
    }

    #[test]
    fn as_adaptive_config_is_none_for_fixed_and_some_for_adaptive() {
        assert!(BatchingConfig::default().as_adaptive_config().is_none());

        let original = AdaptiveBatchConfig {
            adaptive_min_batch_size: 3,
            adaptive_max_batch_size: 30,
            adaptive_window_size: 6,
            adaptive_batch_timeout_ms: 250,
        };
        let round_trip = BatchingConfig::adaptive(original.clone())
            .as_adaptive_config()
            .expect("adaptive variant yields its config");
        assert_eq!(round_trip.adaptive_min_batch_size, 3);
        assert_eq!(round_trip.adaptive_max_batch_size, 30);
        assert_eq!(round_trip.adaptive_window_size, 6);
        assert_eq!(round_trip.adaptive_batch_timeout_ms, 250);
    }

    #[test]
    fn has_renderable_templates_is_false_for_empty_or_whitespace() {
        assert!(!OutputTemplates::default().has_renderable_templates());

        let only_whitespace = OutputTemplates {
            default_template: Some(qc("   ", HashMap::new())),
            routes: HashMap::new(),
        };
        assert!(
            !only_whitespace.has_renderable_templates(),
            "a whitespace-only template with no metadata is not renderable"
        );
    }

    #[test]
    fn has_renderable_templates_is_true_for_body_route_or_metadata() {
        let default_body = OutputTemplates {
            default_template: Some(qc(r#"{"id":1}"#, HashMap::new())),
            routes: HashMap::new(),
        };
        assert!(default_body.has_renderable_templates());

        let mut routes = HashMap::new();
        routes.insert("q1".to_string(), qc(r#"{"id":1}"#, HashMap::new()));
        let route_body = OutputTemplates {
            default_template: None,
            routes,
        };
        assert!(route_body.has_renderable_templates());

        // Metadata-only (empty body) still counts as renderable because the
        // extension metadata must be rendered through Handlebars.
        let mut md = HashMap::new();
        md.insert("x-op".to_string(), "{{operation}}".to_string());
        let metadata_only = OutputTemplates {
            default_template: Some(qc("", md)),
            routes: HashMap::new(),
        };
        assert!(metadata_only.has_renderable_templates());
    }

    #[test]
    fn template_routing_exposes_routes_and_default_template() {
        // No outputTemplates → empty routes and no default template.
        let empty = GrpcReactionConfig::default();
        assert!(empty.routes().is_empty());
        assert!(empty.default_template().is_none());

        let mut routes = HashMap::new();
        routes.insert("q1".to_string(), qc(r#"{"id":1}"#, HashMap::new()));
        let cfg = GrpcReactionConfig {
            output_templates: Some(OutputTemplates {
                default_template: Some(qc(r#"{"d":1}"#, HashMap::new())),
                routes,
            }),
            ..Default::default()
        };
        assert_eq!(cfg.routes().len(), 1);
        assert!(cfg.routes().contains_key("q1"));
        assert!(cfg.default_template().is_some());
    }

    #[test]
    fn output_format_serializes_camel_case_and_defaults_to_canonical() {
        assert_eq!(OutputFormat::default(), OutputFormat::CanonicalJson);
        assert_eq!(
            serde_json::to_value(OutputFormat::CanonicalJson).unwrap(),
            serde_json::json!("canonicalJson")
        );
        assert_eq!(
            serde_json::to_value(OutputFormat::Proto).unwrap(),
            serde_json::json!("proto")
        );
        let parsed: OutputFormat = serde_json::from_value(serde_json::json!("proto")).unwrap();
        assert_eq!(parsed, OutputFormat::Proto);
    }

    #[test]
    fn config_defaults_match_default_helpers() {
        let cfg = GrpcReactionConfig::default();
        assert_eq!(cfg.endpoint, default_grpc_endpoint());
        assert_eq!(cfg.timeout_ms, default_timeout_ms());
        assert_eq!(cfg.max_retries, default_max_retries());
        assert_eq!(
            cfg.connection_retry_attempts,
            default_connection_retry_attempts()
        );
        assert_eq!(
            cfg.initial_connection_timeout_ms,
            default_initial_connection_timeout_ms()
        );
        assert!(cfg.metadata.is_empty());
        assert_eq!(cfg.output_format, OutputFormat::CanonicalJson);
        assert!(cfg.output_templates.is_none());
    }

    #[test]
    fn config_serde_round_trip_preserves_all_fields() {
        let mut metadata = HashMap::new();
        metadata.insert("authorization".to_string(), "Bearer t".to_string());
        let cfg = GrpcReactionConfig {
            endpoint: "grpc://host:1".to_string(),
            timeout_ms: 1234,
            max_retries: 9,
            connection_retry_attempts: 8,
            initial_connection_timeout_ms: 4321,
            metadata,
            batching: BatchingConfig::Fixed {
                batch_size: 64,
                batch_flush_timeout_ms: 32,
            },
            output_format: OutputFormat::Proto,
            output_templates: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        // camelCase keys on the wire.
        assert!(json.get("timeoutMs").is_some());
        assert!(json.get("maxRetries").is_some());
        assert!(json.get("connectionRetryAttempts").is_some());
        assert_eq!(json.get("outputFormat"), Some(&serde_json::json!("proto")));
        // `outputTemplates` is skipped when None.
        assert!(json.get("outputTemplates").is_none());

        let back: GrpcReactionConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn config_deserializes_from_minimal_json_using_defaults() {
        let cfg: GrpcReactionConfig = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(cfg, GrpcReactionConfig::default());
    }
}
