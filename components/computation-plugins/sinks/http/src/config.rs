// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration of the HTTP sink (`drasi.network/http-sink`): fields,
//! published defaults and every check that does not need the graph's edges.

use anyhow::{ensure, Context, Result};
pub use drasi_computation_contracts::PermanentFailurePolicy;
use drasi_computation_contracts::{OutputAliases, SinkOutputs};
pub use drasi_computation_network_common::config::{duration, SecretString};
use drasi_computation_network_common::{
    config::{normalized_headers, timeout_ms, ClientTls, MAX_RETRIES, RESERVED_HEADERS},
    retry::{RetryPolicy, DEFAULT_RETRY_BUDGET_MS, MAX_RETRY_BUDGET_MS},
};
pub use drasi_http_common::reaction::config::{
    AdaptiveBatchConfig, HttpOutputTemplates, HttpReactionConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

/// Default additional attempts after the first.
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// Default host collection byte bound for one batch.
pub const DEFAULT_MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
/// Upper bound on `maxBatchBytes`.
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
/// Default and upper bound on one rendered request body.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Default TCP and TLS connection deadline.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
/// Idle pooled connections are closed after this long (legacy parity).
pub const DEFAULT_POOL_IDLE_TIMEOUT_MS: u64 = 90_000;
/// Idle pooled connections kept per host (legacy parity).
pub const DEFAULT_POOL_MAX_IDLE_PER_HOST: usize = 10;
/// Upper bound on `poolMaxIdlePerHost`.
pub const MAX_POOL_IDLE_PER_HOST: usize = 1024;
/// Responses that complete a delivery unless configured otherwise. `202
/// Accepted` is excluded: it only says the request was queued.
pub const DEFAULT_SUCCESS_STATUS_CODES: [u16; 4] = [200, 201, 203, 204];
/// Upper bound on adaptive batch size accepted by native host batching.
pub const MAX_ADAPTIVE_BATCH_SIZE: usize = 65_536;
/// Upper bound on the adaptive batch timeout accepted by native host batching.
pub const MAX_ADAPTIVE_BATCH_TIMEOUT_MS: u64 = 60_000;

fn retries() -> u32 {
    DEFAULT_MAX_RETRIES
}
fn batch_bytes() -> usize {
    DEFAULT_MAX_BATCH_BYTES
}
fn request_bytes() -> usize {
    MAX_REQUEST_BYTES
}
fn connect_timeout_ms() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn pool_idle_timeout_ms() -> u64 {
    DEFAULT_POOL_IDLE_TIMEOUT_MS
}
fn pool_max_idle_per_host() -> usize {
    DEFAULT_POOL_MAX_IDLE_PER_HOST
}
fn retry_budget_ms() -> u64 {
    DEFAULT_RETRY_BUDGET_MS
}
fn success_status_codes() -> Vec<u16> {
    DEFAULT_SUCCESS_STATUS_CODES.to_vec()
}

/// Settings of the native HTTP graph-change sink. Each bound output may be a
/// source, a transformer or a query port. Secret values never print through
/// `Debug`; serialization returns them because `configuration()` is the
/// persistence hook.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSinkConfig {
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<SecretString>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_headers: BTreeMap<String, SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<HttpOutputTemplates>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "drasi_http_common::reaction::config::adaptive_config_serde"
    )]
    pub adaptive: Option<AdaptiveBatchConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_endpoint: Option<String>,
    #[serde(default = "batch_bytes")]
    pub max_batch_bytes: usize,
    #[serde(default = "request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "pool_idle_timeout_ms")]
    pub pool_idle_timeout_ms: u64,
    #[serde(default = "pool_max_idle_per_host")]
    pub pool_max_idle_per_host: usize,
    #[serde(default = "retries")]
    pub max_retries: u32,
    #[serde(default = "retry_budget_ms")]
    pub retry_budget_ms: u64,
    /// What the graph does with an input the destination can never accept.
    #[serde(default)]
    pub on_permanent_failure: PermanentFailurePolicy,
    #[serde(default = "success_status_codes")]
    pub success_status_codes: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_private_key_pem: Option<SecretString>,
}

impl HttpSinkConfig {
    /// Field descriptions, defaults and ranges published to hosts.
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        use serde_json::json;
        // Constant declarations; `metadata_is_valid` proves they are accepted.
        ConfigDefinition::try_new([
            Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. Every incoming graph edge is accepted whether or not it is aliased; an unaliased output is named after its producing component. The alias names the output in default URLs, template routes and the template context.").constraint(drasi_computation_contracts::ConfigurationConstraint::output_aliases()),
            Field::new("url", String, false, "Fixed default destination (absolute http/https URL). Supply url, baseUrl or both.").default_description("None; url or baseUrl is required."),
            Field::new("baseUrl", String, false, "Base URL for the default /changes/{alias} destination, template URLs and batchEndpoint. Defaults to the origin of url.").default_description("The origin of url."),
            Field::new("token", String, false, "Bearer token sent as Authorization: Bearer <token>.").secret(),
            Field::new("headers", Object, false, "Static request headers. Names are case-insensitive; x-drasi-component, x-drasi-port and x-drasi-delivery-id are supplied by the sink and may not be set. content-type defaults to application/json.").default(json!({})),
            Field::new("secretHeaders", Object, false, "Secret static request headers (for example API keys), merged into headers. The same naming rules apply and a name may not also appear in headers.").secret(),
            Field::new("outputTemplates", Object, false, "Per-output-alias (routes) and default (defaultTemplate) Handlebars templates for each operation (added, updated, deleted): body template, url, method and headers. Template headers may not name the sink's reserved headers."),
            Field::new("adaptive", Object, false, "Adaptive batching bounds (adaptiveMinBatchSize, adaptiveMaxBatchSize, adaptiveWindowSize, adaptiveBatchTimeoutMs); requires batchEndpoint. The host retains inputs until delivery completes."),
            Field::new("batchEndpoint", String, false, "Path relative to the base URL for POST {\"batch\":[...]} delivery; requires adaptive."),
            Field::new("maxBatchBytes", Integer, false, "Host collection byte bound for one batch of inputs.").default(batch_bytes()).minimum(1).maximum(MAX_BATCH_BYTES as u64),
            Field::new("maxRequestBytes", Integer, false, "Maximum rendered request body; adaptive batches split at this boundary.").default(request_bytes()).minimum(1).maximum(MAX_REQUEST_BYTES as u64),
            Field::new("timeoutMs", Integer, false, "Deadline for one HTTP attempt, from connecting until the response body is read.").default(timeout_ms()).minimum(1),
            Field::new("connectTimeoutMs", Integer, false, "Deadline for establishing a TCP and TLS connection.").default(connect_timeout_ms()).minimum(1),
            Field::new("poolIdleTimeoutMs", Integer, false, "Idle pooled connections are closed after this many milliseconds.").default(pool_idle_timeout_ms()).minimum(1),
            Field::new("poolMaxIdlePerHost", Integer, false, "Idle pooled connections kept per host; 0 disables connection reuse.").default(pool_max_idle_per_host()).minimum(0).maximum(MAX_POOL_IDLE_PER_HOST as u64),
            Field::new("maxRetries", Integer, false, "Additional attempts after the first for a retryable failure.").default(retries()).minimum(0).maximum(MAX_RETRIES as u64),
            Field::new("retryBudgetMs", Integer, false, "Total time from the first attempt after which no retry starts; bounds how long one delivery can hold a handle.").default(retry_budget_ms()).minimum(1).maximum(MAX_RETRY_BUDGET_MS),
            Field::on_permanent_failure(),
            Field::new("successStatusCodes", Array, false, "Response statuses (200..=299) that complete a delivery. 202 is excluded by default because it only reports queuing.").default(json!(DEFAULT_SUCCESS_STATUS_CODES)),
            Field::new("tlsCaPem", String, false, "PEM CA certificates trusted in addition to the built-in roots."),
            Field::new("tlsClientCertificatePem", String, false, "PEM client certificate chain for mutual TLS; requires tlsClientPrivateKeyPem."),
            Field::new("tlsClientPrivateKeyPem", String, false, "PEM private key for tlsClientCertificatePem.").secret(),
        ])
        .expect("constant HTTP sink configuration")
    }

    /// Deserializes and validates everything that does not depend on the
    /// graph's incoming edges, including template compilation.
    pub fn parse(value: Value) -> Result<Self> {
        drasi_computation_network_common::config::validate_output_templates(
            &value,
            &["url", "method", "headers"],
        )?;
        let mut config: Self = serde_json::from_value(value)?;
        config.validate()?;
        config.headers = normalized_headers(&config.headers)?;
        let secret_names: BTreeMap<String, String> = config
            .secret_headers
            .iter()
            .map(|(name, value)| (name.clone(), value.expose().to_owned()))
            .collect();
        let normalized = normalized_headers(&secret_names)
            .context("secretHeaders")?
            .into_iter()
            .map(|(name, value)| (name, SecretString::new(value)))
            .collect::<BTreeMap<_, _>>();
        ensure!(
            normalized.len() == config.secret_headers.len(),
            "secretHeaders: duplicate header name ignoring case"
        );
        config.secret_headers = normalized;
        for name in config.secret_headers.keys() {
            ensure!(
                !config.headers.contains_key(name),
                "header {name} is configured in both headers and secretHeaders"
            );
        }
        if let Some(token) = &config.token {
            ensure!(
                !config.headers.contains_key("authorization")
                    && !config.secret_headers.contains_key("authorization"),
                "token supplies the authorization header; do not also configure it"
            );
            ensure!(
                !token.expose().is_empty()
                    && reqwest::header::HeaderValue::from_str(&format!(
                        "Bearer {}",
                        token.expose()
                    ))
                    .is_ok(),
                "token must be a nonempty valid header value"
            );
        }
        crate::render::Renderer::new(config.output_templates.as_ref())?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        self.outputs.validate()?;
        ensure!(
            !self.url.is_empty() || self.base_url.is_some(),
            "supply url or baseUrl"
        );
        if let Some(base) = &self.base_url {
            check_url(base, "baseUrl")?;
        }
        if !self.url.is_empty() {
            check_url(&self.url, "url")?;
        }
        duration(self.timeout_ms, "timeoutMs")?;
        duration(self.connect_timeout_ms, "connectTimeoutMs")?;
        duration(self.pool_idle_timeout_ms, "poolIdleTimeoutMs")?;
        ensure!(
            self.pool_max_idle_per_host <= MAX_POOL_IDLE_PER_HOST,
            "poolMaxIdlePerHost must not exceed {MAX_POOL_IDLE_PER_HOST}"
        );
        ensure!(
            self.max_retries <= MAX_RETRIES,
            "maxRetries must not exceed {MAX_RETRIES}"
        );
        ensure!(
            (1..=MAX_RETRY_BUDGET_MS).contains(&self.retry_budget_ms),
            "retryBudgetMs must be in 1..={MAX_RETRY_BUDGET_MS}"
        );
        ensure!(
            (1..=MAX_BATCH_BYTES).contains(&self.max_batch_bytes),
            "maxBatchBytes must be in 1..={MAX_BATCH_BYTES}"
        );
        ensure!(
            (1..=MAX_REQUEST_BYTES).contains(&self.max_request_bytes),
            "maxRequestBytes must be in 1..={MAX_REQUEST_BYTES}"
        );
        ensure!(
            !self.success_status_codes.is_empty(),
            "successStatusCodes must not be empty"
        );
        let mut seen = std::collections::BTreeSet::new();
        for status in &self.success_status_codes {
            ensure!(
                (200..=299).contains(status),
                "successStatusCodes may contain only 2xx statuses"
            );
            ensure!(seen.insert(*status), "duplicate successStatusCodes entry");
        }
        let legacy = self.reaction_config()?;
        let routes: Vec<String> = self
            .output_templates
            .iter()
            .flat_map(|templates| templates.routes.keys().cloned())
            .collect();
        legacy.validate(&routes, None)?;
        if !self.url.is_empty() {
            drasi_http_common::reaction::config::resolve_http_url(&legacy.base_url, &self.url)
                .context("url")?;
        }
        if let Some(adaptive) = &self.adaptive {
            ensure!(
                adaptive.adaptive_max_batch_size <= MAX_ADAPTIVE_BATCH_SIZE
                    && adaptive.adaptive_batch_timeout_ms <= MAX_ADAPTIVE_BATCH_TIMEOUT_MS,
                "adaptive bounds exceed native batching limits"
            );
        }
        self.validate_template_destinations(&legacy.base_url)?;
        self.client_tls().validate()?;
        Ok(())
    }

    /// Template headers may not name reserved headers, and static template
    /// URLs must resolve on the base origin (HS-07).
    fn validate_template_destinations(&self, base_url: &str) -> Result<()> {
        let Some(templates) = &self.output_templates else {
            return Ok(());
        };
        let configs = templates
            .default_template
            .iter()
            .chain(templates.routes.values());
        for config in configs {
            for spec in [&config.added, &config.updated, &config.deleted]
                .into_iter()
                .flatten()
            {
                for name in spec.extension.headers.keys() {
                    let name: reqwest::header::HeaderName = name.parse()?;
                    ensure!(
                        !RESERVED_HEADERS.contains(&name.as_str()),
                        "template header {name} is supplied by the sink and may not be templated"
                    );
                }
                let url = &spec.extension.url;
                if !url.is_empty() && !url.contains("{{") {
                    drasi_http_common::reaction::config::resolve_http_url(base_url, url)
                        .context("template url")?;
                }
            }
        }
        Ok(())
    }

    /// Checks that depend on the incoming edges: template routes must name
    /// an output of this sink.
    pub fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        let aliases: Vec<String> = outputs.aliases().map(str::to_owned).collect();
        self.reaction_config()?.validate(&aliases, None)
    }

    /// The shared HTTP template configuration used for validation and
    /// template routing. It never carries the token.
    pub fn reaction_config(&self) -> Result<HttpReactionConfig> {
        let base_url = match &self.base_url {
            Some(base) => base.trim_end_matches('/').to_owned(),
            None => reqwest::Url::parse(&self.url)?
                .origin()
                .ascii_serialization(),
        };
        Ok(HttpReactionConfig {
            base_url,
            token: None,
            timeout_ms: self.timeout_ms,
            output_templates: self.output_templates.clone(),
            adaptive: self.adaptive.clone(),
            batch_endpoint: self.batch_endpoint.clone(),
        })
    }

    /// Validated-on-parse client TLS settings.
    pub fn client_tls(&self) -> ClientTls {
        ClientTls {
            ca_pem: self.tls_ca_pem.clone(),
            certificate_pem: self.tls_client_certificate_pem.clone(),
            private_key_pem: self.tls_client_private_key_pem.clone(),
        }
    }

    /// The retry bounds for one delivery.
    pub fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: self.max_retries,
            budget: Duration::from_millis(self.retry_budget_ms),
        }
    }
}

/// An absolute http(s) URL with a host and no credentials, query or fragment
/// on a base URL.
pub(crate) fn check_url(value: &str, field: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(value).with_context(|| format!("{field} is not a URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "{field} must be an http or https URL with a host and no credentials or fragment"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OUTPUT_COMPONENT_ID: &str = "63d14bfc-2827-577e-aed6-d4e1f755df25";

    fn outputs() -> Value {
        json!({"q": {"component": OUTPUT_COMPONENT_ID, "port": "out"}})
    }

    fn with(field: Value) -> Value {
        let mut value = json!({"outputs": outputs(), "url":"http://localhost/events"});
        if let (Some(value), Some(field)) = (value.as_object_mut(), field.as_object()) {
            value.extend(field.clone());
        }
        value
    }

    #[test]
    fn defaults_are_published_exactly() -> Result<()> {
        let config =
            HttpSinkConfig::parse(json!({"outputs": outputs(), "url": "http://localhost/events"}))?;
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "outputs": outputs(), "url": "http://localhost/events", "headers": {},
                "timeoutMs": 60000, "connectTimeoutMs": 10000, "poolIdleTimeoutMs": 90000,
                "poolMaxIdlePerHost": 10, "maxRetries": 3, "retryBudgetMs": 120000,
                "onPermanentFailure": "stall", "successStatusCodes": [200, 201, 203, 204],
                "maxBatchBytes": 16777216, "maxRequestBytes": 33554432
            })
        );
        Ok(())
    }

    #[test]
    fn outputs_are_optional_aliases_and_old_keys_are_gone() -> Result<()> {
        let config = HttpSinkConfig::parse(json!({"url": "http://localhost/events"}))?;
        assert!(config.outputs.is_empty());
        assert!(serde_json::to_value(&config)?.get("outputs").is_none());
        for stale in [
            json!({"queryId": "q", "url": "http://localhost/events"}),
            json!({"producer": OUTPUT_COMPONENT_ID, "url": "http://localhost/events"}),
            json!({"outputs": {"q": {"component": OUTPUT_COMPONENT_ID, "stream": "q/out"}}, "url": "http://localhost/events"}),
            json!({"outputs": {"bad id": {"component": OUTPUT_COMPONENT_ID}}, "url": "http://localhost/events"}),
            json!({}),
        ] {
            assert!(HttpSinkConfig::parse(stale.clone()).is_err(), "{stale}");
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_fields() {
        for field in [
            json!({"maxRequestBytes":0}),
            json!({"headers":{"x-drasi-component":"other"}}),
            json!({"headers":{"X-Drasi-Port":"other"}}),
            json!({"headers":{"x-drasi-delivery-id":"other"}}),
            json!({"secretHeaders":{"x-drasi-delivery-id":"other"}}),
            json!({"headers":{"X-Custom":"one","x-custom":"two"}}),
            json!({"headers":{"x-key":"a"},"secretHeaders":{"X-Key":"b"}}),
            json!({"secretHeaders":{"X-Key":"a","x-key":"b"}}),
            json!({"headers":{"authorization":"Basic x"},"token":"t"}),
            json!({"headers":{"x-custom":"line\nbreak"}}),
            json!({"token":"line\nbreak"}),
            json!({"url":"ftp://localhost/events"}),
            json!({"url":"http://user:pass@localhost/events"}),
            json!({"maxRetries":101}),
            json!({"retryBudgetMs":0}),
            json!({"retryBudgetMs":3_600_001u64}),
            json!({"connectTimeoutMs":0}),
            json!({"poolMaxIdlePerHost":1025}),
            json!({"successStatusCodes":[]}),
            json!({"successStatusCodes":[302]}),
            json!({"successStatusCodes":[200,200]}),
            json!({"tlsCaPem":"not pem"}),
            json!({"tlsClientCertificatePem":"not pem"}),
            json!({"outputTemplates":{"routes":{"q":{"adedd":{"template":"bad"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"templat":"bad"}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"headers":{"X-Drasi-Delivery-Id":"{{sequence}}"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"headers":{"x-drasi-component":"x"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"template":"{{unknownHelper after}}"}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"template":"{{> partial}}"}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"url":"http://elsewhere/x"}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"method":"TRACE"}}}}),
        ] {
            let value = with(field);
            assert!(HttpSinkConfig::parse(value.clone()).is_err(), "{value}");
        }
    }

    #[test]
    fn secrets_normalize_and_never_print() -> Result<()> {
        let config = HttpSinkConfig::parse(with(json!({
            "token": "super-secret-token",
            "secretHeaders": {"X-Api-Key": "super-secret-key"},
            "headers": {"X-Visible": "plain"}
        })))?;
        assert_eq!(
            config
                .secret_headers
                .get("x-api-key")
                .map(SecretString::expose),
            Some("super-secret-key")
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("super-secret"), "{debug}");
        assert!(debug.contains("x-api-key") && debug.contains("plain"));
        let exposed = serde_json::to_value(&config)?;
        assert_eq!(exposed["token"], "super-secret-token");
        assert_eq!(exposed["secretHeaders"]["x-api-key"], "super-secret-key");
        Ok(())
    }
}
