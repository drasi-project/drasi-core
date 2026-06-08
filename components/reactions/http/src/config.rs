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

//! Configuration types for the unified HTTP reaction.
//!
//! The HTTP reaction layers reaction-specific fields (URL, method, headers)
//! on top of the shared template primitives in
//! [`drasi_lib::reactions::common::templates`]. The `template` field
//! inherited from [`TemplateSpec`] is the HTTP request body; `url`,
//! `method`, and `headers` come from the flattened [`HttpCallExt`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use drasi_lib::reactions::common::{
    AdaptiveBatchConfig, OperationType, QueryConfig, TemplateRouting, TemplateSpec,
};

fn default_base_url() -> String {
    "http://localhost".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

/// HTTP-specific extension flattened next to the shared [`TemplateSpec`]'s
/// `template` (request body) field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpCallExt {
    /// URL path (appended to `base_url`) or absolute URL.
    /// Supports Handlebars template syntax.
    #[serde(default)]
    pub url: String,

    /// HTTP method: GET, POST, PUT, DELETE, PATCH (case-insensitive).
    /// Defaults to POST when empty.
    #[serde(default)]
    pub method: String,

    /// Additional HTTP headers as key-value pairs.
    /// Header values support Handlebars templates.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Spec for a single HTTP call: a body template plus URL, method, headers.
pub type HttpCallSpec = TemplateSpec<HttpCallExt>;

/// Per-query configuration: separate [`HttpCallSpec`] for each operation.
pub type HttpQueryConfig = QueryConfig<HttpCallExt>;

/// Optional output-template overrides for the HTTP reaction.
///
/// Grouped under `outputTemplates` for symmetry with the gRPC reaction
/// and the [`TemplateRouting`] trait shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HttpOutputTemplates {
    /// Fallback template used when no per-query route matches.
    /// When neither this nor a matching `routes` entry exists, the
    /// reaction synthesizes a `POST {baseUrl}/changes/{queryId}` call
    /// with the raw diff JSON as the body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<HttpQueryConfig>,

    /// Per-query route overrides keyed by `query_id`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, HttpQueryConfig>,
}

/// HTTP reaction configuration.
///
/// Drives both the standard (per-result) and adaptive (coalesced)
/// runtime paths. Setting [`HttpReactionConfig::adaptive`] to
/// `Some(...)` enables adaptive batching; setting
/// [`HttpReactionConfig::batch_endpoint`] additionally routes any
/// coalesced batch with multi-diff queries to `{baseUrl}{batchEndpoint}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpReactionConfig {
    /// Base URL for HTTP requests.
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Optional authentication token (sent as `Authorization: Bearer …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Per-request timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Optional per-query and default-template overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<HttpOutputTemplates>,

    /// When `Some`, the reaction batches diffs through the adaptive
    /// batcher. When `None`, behaves as per-result delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<AdaptiveBatchConfig>,

    /// Path appended to `base_url` for coalesced batch POSTs.
    ///
    /// Requires `adaptive` to be `Some(...)`. If `batch_endpoint` is set
    /// while `adaptive` is `None`, the reaction fails fast at startup
    /// (logs an error, sets status to `Stopped`, and returns an error)
    /// rather than silently ignoring it. Under adaptive mode with
    /// `batch_endpoint` set to `None`, coalesced batches are still
    /// delivered per-route rather than via a single batch endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_endpoint: Option<String>,
}

impl Default for HttpReactionConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            token: None,
            timeout_ms: default_timeout_ms(),
            output_templates: None,
            adaptive: None,
            batch_endpoint: None,
        }
    }
}

// Empty-routes singleton returned by the `TemplateRouting` impl when no
// `output_templates` are configured.
fn empty_routes() -> &'static HashMap<String, HttpQueryConfig> {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<HashMap<String, HttpQueryConfig>> = OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

impl TemplateRouting<HttpCallExt> for HttpReactionConfig {
    fn routes(&self) -> &HashMap<String, HttpQueryConfig> {
        self.output_templates
            .as_ref()
            .map(|t| &t.routes)
            .unwrap_or_else(|| empty_routes())
    }

    fn default_template(&self) -> Option<&HttpQueryConfig> {
        self.output_templates
            .as_ref()
            .and_then(|t| t.default_template.as_ref())
    }
}

/// Synthesize the today's hard-coded fallback call spec for a query:
/// `POST {base_url}/changes/{query_id}` with the raw diff JSON as the
/// body. Used inside `process_result` when no template matches and as
/// the safety net for render errors.
pub(crate) fn synthesized_default_spec(query_id: &str) -> HttpCallSpec {
    HttpCallSpec {
        template: String::new(),
        extension: HttpCallExt {
            url: format!("/changes/{query_id}"),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    }
}
