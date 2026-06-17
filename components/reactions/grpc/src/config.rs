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

/// Optional Handlebars-based output template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct OutputTemplates {
    /// Default template applied when no per-query override matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,

    /// Per-query overrides keyed by query id.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
}

impl OutputTemplates {
    /// Returns `true` if at least one non-empty template string is configured
    /// (either in the default template or any per-query route). Used to avoid
    /// allocating a Handlebars engine when no template would ever render.
    pub(crate) fn has_renderable_templates(&self) -> bool {
        fn has_nonempty(qc: &QueryConfig) -> bool {
            [qc.added.as_ref(), qc.updated.as_ref(), qc.deleted.as_ref()]
                .into_iter()
                .flatten()
                .any(|spec| !spec.template.trim().is_empty())
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
fn validate_query_config(qc: &QueryConfig) -> anyhow::Result<()> {
    for spec in [qc.added.as_ref(), qc.updated.as_ref(), qc.deleted.as_ref()]
        .into_iter()
        .flatten()
    {
        crate::templates::validate_template(&spec.template)?;
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
        if let Some(templates) = self.output_templates.as_ref() {
            templates.validate(query_ids)?;
        }
        Ok(())
    }
}

fn empty_routes() -> &'static HashMap<String, QueryConfig> {
    static EMPTY: std::sync::OnceLock<HashMap<String, QueryConfig>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

impl TemplateRouting<()> for GrpcReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig> {
        match self.output_templates.as_ref() {
            Some(t) => &t.routes,
            None => empty_routes(),
        }
    }

    fn default_template(&self) -> Option<&QueryConfig> {
        self.output_templates
            .as_ref()
            .and_then(|t| t.default_template.as_ref())
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};

    fn with_templates(
        default: Option<QueryConfig>,
        routes: HashMap<String, QueryConfig>,
    ) -> GrpcReactionConfig {
        GrpcReactionConfig {
            output_templates: Some(OutputTemplates {
                default_template: default,
                routes,
            }),
            ..Default::default()
        }
    }

    fn qc_added(template: &str) -> QueryConfig {
        QueryConfig {
            added: Some(TemplateSpec::new(template)),
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
}
