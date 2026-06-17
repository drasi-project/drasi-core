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

//! Configuration types for the HTTP reaction.
//!
//! The HTTP reaction layers reaction-specific fields (URL, method, headers)
//! on top of the shared template primitives in
//! [`drasi_lib::reactions::common::templates`]. The `template` field
//! inherited from [`TemplateSpec`] is the HTTP request body; `url`,
//! `method`, and `headers` come from the flattened [`HttpCallExt`].

use anyhow::Context;
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
    /// reaction posts the [`crate::output::DefaultChangeNotification`]
    /// envelope to `POST {baseUrl}/changes/{queryId}`.
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
    /// while `adaptive` is `None`, [`HttpReactionConfig::validate`]
    /// rejects the configuration so the builder's `build()` fails fast.
    /// Under adaptive mode with `batch_endpoint` set to `None`, coalesced
    /// batches are still delivered per-route rather than via a single
    /// batch endpoint.
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

/// Last dotted segment of a query id (`source.q1` → `q1`).
fn last_segment(query_id: &str) -> &str {
    query_id.rsplit('.').next().unwrap_or(query_id)
}

/// Select the [`HttpCallSpec`] for `operation` from a per-query config.
fn op_spec(qc: &HttpQueryConfig, operation: OperationType) -> Option<&HttpCallSpec> {
    match operation {
        OperationType::Add => qc.added.as_ref(),
        OperationType::Update => qc.updated.as_ref(),
        OperationType::Delete => qc.deleted.as_ref(),
    }
}

/// Compile a single template string, treating empty as valid (no template).
fn validate_template_str(s: &str) -> anyhow::Result<()> {
    if s.is_empty() {
        return Ok(());
    }
    handlebars::Template::compile(s)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("invalid Handlebars template: {e}"))
}

/// Compile every templatable string in a call spec (body, URL, headers).
fn validate_call_spec(spec: &HttpCallSpec) -> anyhow::Result<()> {
    validate_template_str(&spec.template).context("body template")?;
    validate_template_str(&spec.extension.url).context("url template")?;
    for (key, value) in &spec.extension.headers {
        validate_template_str(value).with_context(|| format!("header '{key}' template"))?;
    }
    Ok(())
}

/// Compile every template in a per-query config.
fn validate_query_config(qc: &HttpQueryConfig) -> anyhow::Result<()> {
    if let Some(spec) = &qc.added {
        validate_call_spec(spec).context("'added' template")?;
    }
    if let Some(spec) = &qc.updated {
        validate_call_spec(spec).context("'updated' template")?;
    }
    if let Some(spec) = &qc.deleted {
        validate_call_spec(spec).context("'deleted' template")?;
    }
    Ok(())
}

impl HttpReactionConfig {
    /// Resolve the call spec for a `(query_id, operation)` pair following
    /// the developer-guide resolution order:
    ///
    /// 1. per-query route keyed by the **full** query id,
    /// 2. per-query route keyed by the **last dotted segment**,
    /// 3. the shared `default_template`.
    ///
    /// Returns `None` when no template applies — the caller then posts the
    /// default [`crate::output::DefaultChangeNotification`] envelope.
    pub(crate) fn resolve_call_spec(
        &self,
        query_id: &str,
        operation: OperationType,
    ) -> Option<&HttpCallSpec> {
        let routes = self.routes();

        if let Some(spec) = routes.get(query_id).and_then(|qc| op_spec(qc, operation)) {
            return Some(spec);
        }

        let segment = last_segment(query_id);
        if segment != query_id {
            if let Some(spec) = routes.get(segment).and_then(|qc| op_spec(qc, operation)) {
                return Some(spec);
            }
        }

        self.default_template()
            .and_then(|qc| op_spec(qc, operation))
    }

    /// Validate the configuration against the reaction's subscribed
    /// `query_ids`. Called by the builder's `build()` so misconfiguration
    /// fails at construction rather than at dispatch.
    ///
    /// Checks:
    /// 1. `batch_endpoint` is only set when `adaptive` is enabled.
    /// 2. Every body / URL / header template compiles.
    /// 3. Every `routes` key matches a subscribed query id (or its last
    ///    dotted segment).
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        if self.batch_endpoint.is_some() && self.adaptive.is_none() {
            anyhow::bail!(
                "`batchEndpoint` requires `adaptive` (coalesced batch) mode; \
                 set `adaptive` to enable batch delivery"
            );
        }

        if let Some(templates) = &self.output_templates {
            if let Some(default) = &templates.default_template {
                validate_query_config(default).context("default template")?;
            }
            for (key, qc) in &templates.routes {
                validate_query_config(qc).with_context(|| format!("route '{key}'"))?;
                let matches = query_ids.iter().any(|q| q == key || last_segment(q) == key);
                if !matches {
                    anyhow::bail!(
                        "output template route '{key}' does not match any subscribed query id"
                    );
                }
            }
        }

        Ok(())
    }
}
