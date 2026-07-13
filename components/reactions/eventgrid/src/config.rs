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

//! Configuration types for the Azure Event Grid reaction.
//!
//! The Event Grid reaction layers reaction-specific fields (endpoint, access
//! key, schema, format) on top of the shared template primitives in
//! [`drasi_lib::reactions::common::templates`]. Per-query templates use the
//! [`EventGridTemplateExt`] extension so each template can carry `metadata`
//! that becomes CloudEvent extension attributes.

use std::collections::HashMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use url::Url;

pub use drasi_lib::reactions::common::{OperationType, QueryConfig, TemplateRouting, TemplateSpec};

/// Default request timeout in milliseconds.
pub fn default_timeout_ms() -> u64 {
    10_000
}

/// AAD token scope for the Event Grid data plane.
pub const EVENT_GRID_SCOPE: &str = "https://eventgrid.azure.net/.default"; // DevSkim: ignore DS137138

/// Output format for events published to Event Grid.
///
/// Mirrors the Drasi Platform Event Grid reaction's `format` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[schema(as = reaction::eventgrid::OutputFormat)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// One event carrying the raw packed `QueryResult`.
    #[default]
    Packed,
    /// One event per result-set change (add/update/delete).
    Unpacked,
    /// Handlebars-templated event `data` per query per operation.
    Template,
}

/// Wire schema used to encode events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, utoipa::ToSchema)]
#[schema(as = reaction::eventgrid::EventGridSchema)]
pub enum EventGridSchema {
    /// CloudEvents 1.0 schema (default). Metadata becomes extension attributes.
    #[default]
    CloudEvents,
    /// Native Event Grid schema. Has no extension-attribute concept.
    EventGrid,
}

impl EventGridSchema {
    /// HTTP `Content-Type` for a batch (JSON array) of events in this schema.
    pub fn content_type(&self) -> &'static str {
        match self {
            EventGridSchema::CloudEvents => "application/cloudevents-batch+json",
            EventGridSchema::EventGrid => "application/json",
        }
    }
}

/// Template extension carrying per-template `metadata` (CloudEvent extension
/// attributes). Flattened next to the shared [`TemplateSpec`]'s `template` field.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridTemplateExt {
    /// Key-value pairs applied as CloudEvent extension attributes.
    /// Ignored (with a warning) when the wire schema is native `EventGrid`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

/// Spec for a single templated event: a `data` body template plus `metadata`.
pub type EventGridTemplateSpec = TemplateSpec<EventGridTemplateExt>;

/// Per-query configuration: separate [`EventGridTemplateSpec`] per operation.
pub type EventGridQueryConfig = QueryConfig<EventGridTemplateExt>;

/// Optional output-template overrides for the Event Grid reaction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridOutputTemplates {
    /// Fallback template used when no per-query route matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<EventGridQueryConfig>,

    /// Per-query route overrides keyed by `query_id`.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, EventGridQueryConfig>,
}

/// Event Grid reaction configuration.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct EventGridReactionConfig {
    /// Full Event Grid custom-topic events URL, e.g.
    /// `https://<topic>.<region>.eventgrid.azure.net/api/events`.
    pub endpoint: String,

    /// Topic access key (sent as the `aeg-sas-key` header). When absent, the
    /// reaction authenticates with AAD via its identity provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key: Option<String>,

    /// Wire schema (CloudEvents or EventGrid). Defaults to CloudEvents.
    #[serde(default)]
    pub schema: EventGridSchema,

    /// Output format (packed/unpacked/template). Defaults to packed.
    #[serde(default)]
    pub format: OutputFormat,

    /// Per-request timeout in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Optional per-query and default-template overrides (used in template mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<EventGridOutputTemplates>,

    /// Permit plaintext `http` endpoints. **Disabled by default**: with an
    /// access key an `http` endpoint would transmit the `aeg-sas-key` credential
    /// in cleartext. Opt in only for local test servers (e.g. wiremock).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_http: bool,
}

impl Default for EventGridReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            access_key: None,
            schema: EventGridSchema::default(),
            format: OutputFormat::default(),
            timeout_ms: default_timeout_ms(),
            output_templates: None,
            allow_http: false,
        }
    }
}

impl std::fmt::Debug for EventGridReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventGridReactionConfig")
            .field("endpoint", &self.endpoint)
            .field(
                "access_key",
                &self.access_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field("schema", &self.schema)
            .field("format", &self.format)
            .field("timeout_ms", &self.timeout_ms)
            .field("output_templates", &self.output_templates)
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

// Empty-routes singleton returned by the `TemplateRouting` impl when no
// `output_templates` are configured.
fn empty_routes() -> &'static HashMap<String, EventGridQueryConfig> {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<HashMap<String, EventGridQueryConfig>> = OnceLock::new();
    EMPTY.get_or_init(HashMap::new)
}

impl TemplateRouting<EventGridTemplateExt> for EventGridReactionConfig {
    fn routes(&self) -> &HashMap<String, EventGridQueryConfig> {
        self.output_templates
            .as_ref()
            .map(|t| &t.routes)
            .unwrap_or_else(|| empty_routes())
    }

    fn default_template(&self) -> Option<&EventGridQueryConfig> {
        self.output_templates
            .as_ref()
            .and_then(|t| t.default_template.as_ref())
    }

    /// Resolve the template spec following the resolution order:
    /// 1. per-query route keyed by the **full** query id,
    /// 2. per-query route keyed by the **last dotted segment**,
    /// 3. the shared `default_template`.
    fn get_template_spec(
        &self,
        query_id: &str,
        operation: OperationType,
    ) -> Option<&EventGridTemplateSpec> {
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
pub(crate) fn last_segment(query_id: &str) -> &str {
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

/// Compile every template in a per-query config.
fn validate_query_config(qc: &EventGridQueryConfig) -> anyhow::Result<()> {
    for (name, spec) in [
        ("added", &qc.added),
        ("updated", &qc.updated),
        ("deleted", &qc.deleted),
    ] {
        if let Some(spec) = spec {
            validate_template_str(&spec.template).with_context(|| format!("'{name}' template"))?;
        }
    }
    Ok(())
}

impl EventGridReactionConfig {
    /// Validate the configuration against the reaction's subscribed
    /// `query_ids`. Called by the builder's `build()` so misconfiguration fails
    /// at construction rather than at dispatch.
    pub fn validate(
        &self,
        query_ids: &[String],
        priority_queue_capacity: Option<usize>,
    ) -> anyhow::Result<()> {
        validate_endpoint(&self.endpoint, self.allow_http).context("endpoint")?;

        if self.timeout_ms == 0 {
            anyhow::bail!("`timeoutMs` must be greater than 0");
        }

        if matches!(priority_queue_capacity, Some(0)) {
            anyhow::bail!("`priorityQueueCapacity` must be greater than 0");
        }

        // Template format requires at least one template (route or default).
        if self.format == OutputFormat::Template {
            let has_templates = self
                .output_templates
                .as_ref()
                .map(|t| t.default_template.is_some() || !t.routes.is_empty())
                .unwrap_or(false);
            if !has_templates {
                anyhow::bail!(
                    "`template` format requires at least one query template or a default template"
                );
            }
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

/// Validate that `endpoint` is an absolute URL with a host. Requires `https`
/// unless `allow_http` is explicitly set (local test servers only): an `http`
/// endpoint would transmit an access-key credential in cleartext.
fn validate_endpoint(endpoint: &str, allow_http: bool) -> anyhow::Result<()> {
    if endpoint.is_empty() {
        anyhow::bail!("must not be empty");
    }
    let url = Url::parse(endpoint).context("invalid URL")?;
    match url.scheme() {
        "https" => {}
        // Plaintext http is rejected by default because it would leak the
        // `aeg-sas-key` credential; opt in via `allow_http` for local testing.
        "http" if allow_http => {}
        "http" => anyhow::bail!(
            "http endpoints are not permitted (the access key would be sent in cleartext); use https or set `allowHttp` for local testing"
        ),
        scheme => anyhow::bail!("unsupported URL scheme '{scheme}'; expected https"),
    }
    if url.host_str().is_none() {
        anyhow::bail!("URL must include a host");
    }
    Ok(())
}
