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

//! Configuration types for the unified gRPC reaction.

use drasi_lib::reactions::common::{
    AdaptiveBatchConfig, QueryConfig, TemplateRouting,
};
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

/// Batching strategy used by the gRPC reaction.
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
    Adaptive(AdaptiveBatchConfig),
}

impl Default for BatchingConfig {
    fn default() -> Self {
        BatchingConfig::Fixed {
            batch_size: default_batch_size(),
            batch_flush_timeout_ms: default_batch_flush_timeout_ms(),
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
}

/// Unified gRPC reaction configuration.
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
