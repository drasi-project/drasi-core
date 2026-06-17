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
use reqwest::{
    header::{HeaderName, HeaderValue},
    Method, Url,
};
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
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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
/// Drives either the standard (per-result) runtime path or the adaptive
/// coalesced batch path. Setting [`HttpReactionConfig::adaptive`] to
/// `Some(...)` enables adaptive batching and requires
/// [`HttpReactionConfig::batch_endpoint`] so every adaptive batch has one
/// downstream POST target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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
    ///
    /// In standard (per-result) mode the `url`, `method`, `headers`, and body
    /// `template` all apply. In adaptive (batch) mode only the body `template`
    /// is used to render each item placed in the
    /// [`crate::output::BatchEnvelope`]; `url`/`method`/`headers` do not apply
    /// because a batch is a single POST to `batch_endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<HttpOutputTemplates>,

    /// When `Some`, the reaction batches diffs through the adaptive
    /// batcher. When `None`, behaves as per-result delivery.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "adaptive_config_serde"
    )]
    pub adaptive: Option<AdaptiveBatchConfig>,

    /// Path appended to `base_url` for coalesced batch POSTs.
    ///
    /// Requires `adaptive` to be `Some(...)`; adaptive mode also requires
    /// this field so batched delivery is an explicit HTTP contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_endpoint: Option<String>,
}

mod adaptive_config_serde {
    use super::AdaptiveBatchConfig;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields, rename_all = "camelCase")]
    struct AdaptiveBatchConfigWire {
        #[serde(default)]
        adaptive_min_batch_size: Option<usize>,
        #[serde(default)]
        adaptive_max_batch_size: Option<usize>,
        #[serde(default)]
        adaptive_window_size: Option<usize>,
        #[serde(default)]
        adaptive_batch_timeout_ms: Option<u64>,
    }

    impl From<&AdaptiveBatchConfig> for AdaptiveBatchConfigWire {
        fn from(value: &AdaptiveBatchConfig) -> Self {
            Self {
                adaptive_min_batch_size: Some(value.adaptive_min_batch_size),
                adaptive_max_batch_size: Some(value.adaptive_max_batch_size),
                adaptive_window_size: Some(value.adaptive_window_size),
                adaptive_batch_timeout_ms: Some(value.adaptive_batch_timeout_ms),
            }
        }
    }

    impl From<AdaptiveBatchConfigWire> for AdaptiveBatchConfig {
        fn from(value: AdaptiveBatchConfigWire) -> Self {
            let defaults = AdaptiveBatchConfig::default();
            Self {
                adaptive_min_batch_size: value
                    .adaptive_min_batch_size
                    .unwrap_or(defaults.adaptive_min_batch_size),
                adaptive_max_batch_size: value
                    .adaptive_max_batch_size
                    .unwrap_or(defaults.adaptive_max_batch_size),
                adaptive_window_size: value
                    .adaptive_window_size
                    .unwrap_or(defaults.adaptive_window_size),
                adaptive_batch_timeout_ms: value
                    .adaptive_batch_timeout_ms
                    .unwrap_or(defaults.adaptive_batch_timeout_ms),
            }
        }
    }

    pub fn serialize<S>(
        value: &Option<AdaptiveBatchConfig>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value
            .as_ref()
            .map(AdaptiveBatchConfigWire::from)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<AdaptiveBatchConfig>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<AdaptiveBatchConfigWire>::deserialize(deserializer)
            .map(|value| value.map(AdaptiveBatchConfig::from))
    }
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

    /// Resolve the call spec for a `(query_id, operation)` pair following the
    /// developer-guide resolution order:
    ///
    /// 1. per-query route keyed by the **full** query id,
    /// 2. per-query route keyed by the **last dotted segment**,
    /// 3. the shared `default_template`.
    ///
    /// Overrides the trait default (which only does steps 1 and 3) so the
    /// public routing API matches what the reaction uses at dispatch time.
    /// Returns `None` when no template applies — the caller then posts the
    /// default [`crate::output::DefaultChangeNotification`] envelope.
    fn get_template_spec(&self, query_id: &str, operation: OperationType) -> Option<&HttpCallSpec> {
        let routes = self.routes();

        if let Some(spec) = routes
            .get(query_id)
            .and_then(|qc| Self::get_spec_from_config(qc, operation))
        {
            return Some(spec);
        }

        let segment = last_segment(query_id);
        if segment != query_id {
            if let Some(spec) = routes
                .get(segment)
                .and_then(|qc| Self::get_spec_from_config(qc, operation))
            {
                return Some(spec);
            }
        }

        self.default_template()
            .and_then(|qc| Self::get_spec_from_config(qc, operation))
    }
}

/// Last dotted segment of a query id (`source.q1` → `q1`).
fn last_segment(query_id: &str) -> &str {
    query_id.rsplit('.').next().unwrap_or(query_id)
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
    parse_http_method(&spec.extension.method).context("HTTP method")?;
    for (key, value) in &spec.extension.headers {
        HeaderName::from_bytes(key.as_bytes())
            .with_context(|| format!("invalid HTTP header name '{key}'"))?;
        validate_template_str(value).with_context(|| format!("header '{key}' template"))?;
        if !value.contains("{{") {
            HeaderValue::from_str(value)
                .with_context(|| format!("invalid static value for HTTP header '{key}'"))?;
        }
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
    /// Validate the configuration against the reaction's subscribed
    /// `query_ids`. Called by the builder's `build()` so misconfiguration
    /// fails at construction rather than at dispatch.
    ///
    /// Checks:
    /// 1. URL, timeout, adaptive, and batch-endpoint invariants.
    /// 2. Every body / URL / header template compiles.
    /// 3. Every static HTTP method and header is valid.
    /// 4. Every `routes` key matches a subscribed query id (or its last
    ///    dotted segment).
    pub fn validate(
        &self,
        query_ids: &[String],
        priority_queue_capacity: Option<usize>,
    ) -> anyhow::Result<()> {
        validate_base_url(&self.base_url).context("baseUrl")?;

        if self.timeout_ms == 0 {
            anyhow::bail!("`timeoutMs` must be greater than 0");
        }

        if matches!(priority_queue_capacity, Some(0)) {
            anyhow::bail!("`priorityQueueCapacity` must be greater than 0");
        }

        match (&self.adaptive, &self.batch_endpoint) {
            (None, Some(_)) => {
                anyhow::bail!(
                    "`batchEndpoint` requires `adaptive` (coalesced batch) mode; \
                     set `adaptive` to enable batch delivery"
                );
            }
            (Some(_), None) => {
                anyhow::bail!(
                    "`adaptive` mode requires `batchEndpoint`; HTTP adaptive delivery \
                     sends coalesced batches to a single endpoint"
                );
            }
            _ => {}
        }

        if let Some(batch_endpoint) = &self.batch_endpoint {
            validate_batch_endpoint(batch_endpoint).context("batchEndpoint")?;
        }

        if let Some(adaptive) = &self.adaptive {
            validate_adaptive(adaptive).context("adaptive")?;
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

fn validate_base_url(base_url: &str) -> anyhow::Result<()> {
    let url = Url::parse(base_url).context("invalid URL")?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => anyhow::bail!("unsupported URL scheme '{scheme}'; expected http or https"),
    }
    if url.host_str().is_none() {
        anyhow::bail!("URL must include a host");
    }
    if url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("URL must not include a query string or fragment");
    }
    Ok(())
}

fn validate_batch_endpoint(endpoint: &str) -> anyhow::Result<()> {
    if endpoint.is_empty() {
        anyhow::bail!("must not be empty");
    }
    if !endpoint.starts_with('/') || endpoint.starts_with("//") {
        anyhow::bail!("must be an absolute path beginning with a single '/'");
    }
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        anyhow::bail!("must be a path, not an absolute URL");
    }
    if endpoint.contains("{{") {
        anyhow::bail!("does not support Handlebars templates");
    }
    Ok(())
}

fn validate_adaptive(adaptive: &AdaptiveBatchConfig) -> anyhow::Result<()> {
    if adaptive.adaptive_min_batch_size == 0 {
        anyhow::bail!("`adaptiveMinBatchSize` must be greater than 0");
    }
    if adaptive.adaptive_max_batch_size == 0 {
        anyhow::bail!("`adaptiveMaxBatchSize` must be greater than 0");
    }
    if adaptive.adaptive_min_batch_size > adaptive.adaptive_max_batch_size {
        anyhow::bail!("`adaptiveMinBatchSize` must be <= `adaptiveMaxBatchSize`");
    }
    if !(1..=255).contains(&adaptive.adaptive_window_size) {
        anyhow::bail!("`adaptiveWindowSize` must be in the range 1..=255");
    }
    if adaptive.adaptive_batch_timeout_ms == 0 {
        anyhow::bail!("`adaptiveBatchTimeoutMs` must be greater than 0");
    }
    Ok(())
}

pub(crate) fn parse_http_method(method: &str) -> anyhow::Result<Method> {
    match method.trim().to_ascii_uppercase().as_str() {
        "" | "POST" => Ok(Method::POST),
        "GET" => Ok(Method::GET),
        "PUT" => Ok(Method::PUT),
        "DELETE" => Ok(Method::DELETE),
        "PATCH" => Ok(Method::PATCH),
        other => anyhow::bail!(
            "unsupported HTTP method '{other}'; expected GET, POST, PUT, PATCH, or DELETE"
        ),
    }
}

pub(crate) fn resolve_http_url(base_url: &str, rendered_url: &str) -> anyhow::Result<String> {
    if rendered_url.starts_with("http://") || rendered_url.starts_with("https://") {
        let base = Url::parse(base_url).context("invalid baseUrl")?;
        let resolved = Url::parse(rendered_url).context("invalid rendered URL")?;
        if base.scheme() != resolved.scheme()
            || base.host_str() != resolved.host_str()
            || base.port_or_known_default() != resolved.port_or_known_default()
        {
            anyhow::bail!(
                "rendered URL '{rendered_url}' must match baseUrl scheme, host, and port"
            );
        }
        Ok(rendered_url.to_string())
    } else {
        Ok(format!("{base_url}{rendered_url}"))
    }
}
