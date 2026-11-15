// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration for `drasi.network/grpc-sink`.
//!
//! Every field, its default, range, secrecy and update boundary is listed in
//! `CONFIGURATION.md`. [`GrpcSinkConfig::definition`] publishes the same
//! defaults the constructor applies.

use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{OutputAliases, SinkOutputs};
pub use drasi_computation_network_common::config::FailurePolicy;
use drasi_computation_network_common::config::{
    duration, normalized_headers, timeout_ms, ClientTls, SecretString, MAX_RETRIES,
    RESERVED_HEADERS,
};
use drasi_computation_network_common::retry::{DEFAULT_RETRY_BUDGET_MS, MAX_RETRY_BUDGET_MS};
pub use drasi_grpc_common::output::config::{
    BatchingConfig, GrpcQueryConfig, GrpcReactionConfig, OutputFormat, OutputTemplates,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Largest fixed or adaptive batch, in changes.
pub const MAX_BATCH_SIZE: usize = 65_536;
/// Largest host collection delay, in milliseconds.
pub const MAX_FLUSH_TIMEOUT_MS: u64 = 60_000;
/// Largest host collection byte bound.
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
/// Largest encoded request.
pub const MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
/// Largest HTTP/2 keepalive interval or timeout, in milliseconds.
pub const MAX_KEEPALIVE_MS: u64 = 3_600_000;

fn endpoint() -> String {
    "grpc://localhost:50052".into()
}
fn retries() -> u32 {
    5
}
fn connect_timeout() -> u64 {
    10_000
}
fn retry_budget() -> u64 {
    DEFAULT_RETRY_BUDGET_MS
}
fn keepalive_interval() -> u64 {
    30_000
}
fn keepalive_timeout() -> u64 {
    20_000
}
fn batch_size() -> usize {
    1
}
fn flush_timeout() -> u64 {
    100
}
fn batch_bytes() -> usize {
    16 * 1024 * 1024
}
fn request_bytes() -> usize {
    4 * 1024 * 1024
}
fn is_false(value: &bool) -> bool {
    !value
}

/// Request compression.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Compression {
    /// Requests are sent uncompressed.
    #[default]
    None,
    /// Requests are gzip-compressed; gzip responses are accepted.
    Gzip,
}

/// Settings for one native gRPC graph-change sink. Each bound output may be a
/// source, a transformer or a query port.
///
/// Secret fields (`secretMetadata`, `tlsClientPrivateKeyPem`) are supplied as
/// host secret references and never printed by `Debug`. `configuration()`
/// returns them because it is the persistence hook.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcSinkConfig {
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default = "endpoint")]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_private_key_pem: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_domain_name: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_metadata: BTreeMap<String, SecretString>,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub require_connected_at_start: bool,
    #[serde(default = "retries")]
    pub max_retries: u32,
    #[serde(default = "retry_budget")]
    pub retry_budget_ms: u64,
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    #[serde(default = "keepalive_interval")]
    pub keepalive_interval_ms: u64,
    #[serde(default = "keepalive_timeout")]
    pub keepalive_timeout_ms: u64,
    #[serde(default)]
    pub compression: Compression,
    #[serde(default = "batch_size")]
    pub batch_size: usize,
    #[serde(default = "flush_timeout")]
    pub batch_flush_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batching: Option<BatchingConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_templates: Option<OutputTemplates>,
    #[serde(default = "batch_bytes")]
    pub max_batch_bytes: usize,
    #[serde(default = "request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default)]
    pub output_format: OutputFormat,
}

/// The validated destination address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    /// `http://host:port` or `https://host:port`, as tonic expects it.
    pub uri: String,
    pub tls: bool,
}

impl GrpcSinkConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        use serde_json::json;
        let retries_max = MAX_RETRIES as u64;
        ConfigDefinition::try_new([
            Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. Every incoming graph edge is accepted whether or not it is aliased; the graph supplies the producing component, output port and stream. An unaliased output is named after its producing component. The alias names the output in templates.").constraint(drasi_computation_contracts::ConfigurationConstraint::output_aliases()),
            Field::new("endpoint", String, false, "Receiver address: grpc:// or http:// for plaintext, grpcs:// or https:// for TLS. Host and optional port only; no path, credentials or query.").default(endpoint()),
            Field::new("tlsCaPem", String, false, "PEM CA bundle trusted in addition to the bundled Mozilla roots for an https:// or grpcs:// endpoint."),
            Field::new("tlsClientCertificatePem", String, false, "PEM client certificate chain for mutual TLS. Requires tlsClientPrivateKeyPem and a TLS endpoint."),
            Field::new("tlsClientPrivateKeyPem", String, false, "PEM private key for tlsClientCertificatePem.").secret(),
            Field::new("tlsDomainName", String, false, "Server name verified against the receiver certificate instead of the endpoint host. Requires a TLS endpoint."),
            Field::new("metadata", Object, false, "ASCII gRPC request metadata (name to value), sent as headers and in the request's metadata map. Names are case-insensitive; x-drasi-component, x-drasi-port and x-drasi-delivery-id are supplied automatically and may not be set.").default(json!({})),
            Field::new("secretMetadata", Object, false, "Credential metadata (name to value), sent only as request headers and never logged. Names may not repeat metadata or template metadata names or use reserved names.").secret(),
            Field::new("timeoutMs", Integer, false, "Deadline for each ProcessChanges call in milliseconds, propagated to the receiver as grpc-timeout. Connection setup is bounded separately by connectTimeoutMs.").default(timeout_ms()).minimum(1),
            Field::new("connectTimeoutMs", Integer, false, "Bound on each connection attempt (TCP, TLS and HTTP/2 setup), in milliseconds.").default(connect_timeout()).minimum(1),
            Field::new("requireConnectedAtStart", Boolean, false, "Connect during Start, retrying within the retry policy and the Start deadline. When false, Start does not contact the receiver and the first delivery connects.").default(false),
            Field::new("maxRetries", Integer, false, "Additional attempts for one request after a retryable failure.").default(retries()).minimum(0).maximum(retries_max),
            Field::new("retryBudgetMs", Integer, false, "Total time in milliseconds after the first attempt of one request within which retries may start. Also bounds connection retries at Start.").default(retry_budget()).minimum(1).maximum(MAX_RETRY_BUDGET_MS),
            Field::new("failurePolicy", String, false, "strict fails the input so the host retries it; skip logs a sanitized record and completes the input. Applies to exhausted retries, permanent rejections, incomplete acknowledgements, template failures and oversized changes.").default(json!(FailurePolicy::default())).allowed([json!(FailurePolicy::Strict), json!(FailurePolicy::Skip)]),
            Field::new("keepaliveIntervalMs", Integer, false, "HTTP/2 PING interval while a call is open, in milliseconds; 0 disables keepalive.").default(keepalive_interval()).minimum(0).maximum(MAX_KEEPALIVE_MS),
            Field::new("keepaliveTimeoutMs", Integer, false, "Time to wait for a keepalive PING acknowledgement before closing the connection, in milliseconds.").default(keepalive_timeout()).minimum(1).maximum(MAX_KEEPALIVE_MS),
            Field::new("compression", String, false, "Request compression: none or gzip.").default(json!(Compression::default())).allowed([json!(Compression::None), json!(Compression::Gzip)]),
            Field::new("batchSize", Integer, false, "Fixed batch item target; may not be combined with batching.").default(batch_size()).minimum(1).maximum(MAX_BATCH_SIZE as u64),
            Field::new("batchFlushTimeoutMs", Integer, false, "Host collection deadline in fixed mode; sparse batches flush without acknowledging early.").default(flush_timeout()).minimum(1).maximum(MAX_FLUSH_TIMEOUT_MS),
            Field::new("batching", Object, false, "Fixed or adaptive batching configuration; mutually exclusive with flat batchSize/batchFlushTimeoutMs."),
            Field::new("outputTemplates", Object, false, "Per-output-alias and default body and request-metadata templates. Templates replace only the payload; typed images, identity and operation are never rewritten."),
            Field::new("maxBatchBytes", Integer, false, "Host collection byte bound; larger individual inputs use direct handling.").default(batch_bytes()).minimum(1).maximum(MAX_BATCH_BYTES as u64),
            Field::new("maxRequestBytes", Integer, false, "Maximum encoded protobuf request; compatible changes split before this limit and a larger single change fails under failurePolicy.").default(request_bytes()).minimum(1).maximum(MAX_REQUEST_BYTES as u64),
            Field::new("outputFormat", String, false, "canonicalJson includes the canonical JSON payload; proto sends the typed before/after fields without that payload.").default(json!(OutputFormat::default())).allowed([json!(OutputFormat::CanonicalJson), json!(OutputFormat::Proto)]),
        ])
        .unwrap_or_else(|error| unreachable!("constant gRPC sink configuration: {error}"))
    }

    /// The exposed configuration. With a `batching` object the flat
    /// `batchSize`/`batchFlushTimeoutMs` fields are omitted, so the value can
    /// be parsed again.
    pub fn to_value(&self) -> Result<Value> {
        let mut value = serde_json::to_value(self)?;
        if self.batching.is_some() {
            if let Some(object) = value.as_object_mut() {
                object.remove("batchSize");
                object.remove("batchFlushTimeoutMs");
            }
        }
        Ok(value)
    }

    pub fn parse(value: Value) -> Result<Self> {
        drasi_computation_network_common::config::validate_output_templates(&value, &["metadata"])?;
        ensure!(
            value.get("batching").is_none()
                || (value.get("batchSize").is_none() && value.get("batchFlushTimeoutMs").is_none()),
            "batching cannot be combined with flat batch settings"
        );
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.outputs.validate()?;
        let destination = self.destination()?;
        let tls = self.client_tls();
        ensure!(
            destination.tls || (!tls.is_configured() && self.tls_domain_name.is_none()),
            "TLS settings require an https:// or grpcs:// endpoint"
        );
        tls.validate()?;
        if let Some(domain) = &self.tls_domain_name {
            ensure!(
                !domain.is_empty()
                    && domain.len() <= 253
                    && domain
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | ':')),
                "tlsDomainName must be a DNS name or IP address"
            );
        }
        duration(self.timeout_ms, "timeoutMs")?;
        duration(self.connect_timeout_ms, "connectTimeoutMs")?;
        duration(self.batch_flush_timeout_ms, "batchFlushTimeoutMs")?;
        ensure!(
            self.max_retries <= MAX_RETRIES,
            "maxRetries must be at most {MAX_RETRIES}"
        );
        ensure!(
            (1..=MAX_RETRY_BUDGET_MS).contains(&self.retry_budget_ms),
            "retryBudgetMs must be in 1..={MAX_RETRY_BUDGET_MS}"
        );
        ensure!(
            self.keepalive_interval_ms <= MAX_KEEPALIVE_MS
                && (1..=MAX_KEEPALIVE_MS).contains(&self.keepalive_timeout_ms),
            "keepalive settings exceed supported bounds"
        );
        ensure!(
            (1..=MAX_BATCH_SIZE).contains(&self.batch_size)
                && self.batch_flush_timeout_ms <= MAX_FLUSH_TIMEOUT_MS,
            "invalid fixed batching bounds"
        );
        ensure!(
            (1..=MAX_BATCH_BYTES).contains(&self.max_batch_bytes),
            "invalid maxBatchBytes"
        );
        ensure!(
            (1..=MAX_REQUEST_BYTES).contains(&self.max_request_bytes),
            "invalid maxRequestBytes"
        );
        let metadata = self.normalized_metadata()?;
        let secret = self.secret_headers()?;
        let template_names = self.template_metadata_names()?;
        for name in secret.keys() {
            ensure!(
                !metadata.contains_key(name) && !template_names.contains(name),
                "secretMetadata name {name} repeats a metadata or template metadata name"
            );
        }
        let reaction = self.reaction_config();
        let routes: Vec<String> = self
            .output_templates
            .iter()
            .flat_map(|templates| templates.routes.keys().cloned())
            .collect();
        reaction.validate(&routes)?;
        match reaction.batching {
            BatchingConfig::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => ensure!(
                batch_size <= MAX_BATCH_SIZE && batch_flush_timeout_ms <= MAX_FLUSH_TIMEOUT_MS,
                "fixed batching bounds exceeded"
            ),
            BatchingConfig::Adaptive {
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
                ..
            } => ensure!(
                adaptive_max_batch_size <= MAX_BATCH_SIZE
                    && adaptive_window_size <= 255
                    && adaptive_batch_timeout_ms <= MAX_FLUSH_TIMEOUT_MS,
                "adaptive batching bounds exceeded"
            ),
        }
        Ok(())
    }

    /// Checks that depend on the incoming edges: template routes must name an
    /// output of this sink (its full alias or the alias's last dotted segment).
    pub fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        let aliases: Vec<String> = outputs.aliases().map(str::to_owned).collect();
        self.reaction_config().validate(&aliases)
    }

    /// The scheme-mapped receiver address.
    pub fn destination(&self) -> Result<Destination> {
        let (scheme, rest) = self
            .endpoint
            .split_once("://")
            .context("endpoint must include a scheme")?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "grpc" | "http" => false,
            "grpcs" | "https" => true,
            other => {
                anyhow::bail!("unsupported endpoint scheme {other}; use grpc, http, grpcs or https")
            }
        };
        let uri = format!("{}://{rest}", if tls { "https" } else { "http" });
        let url = reqwest::Url::parse(&uri).context("invalid endpoint")?;
        ensure!(
            url.host_str().is_some()
                && url.path() == "/"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "endpoint must name only a host and optional port"
        );
        Ok(Destination { uri, tls })
    }

    pub fn client_tls(&self) -> ClientTls {
        ClientTls {
            ca_pem: self.tls_ca_pem.clone(),
            certificate_pem: self.tls_client_certificate_pem.clone(),
            private_key_pem: self.tls_client_private_key_pem.clone(),
        }
    }

    /// Configured metadata with lowercase names, validated as gRPC ASCII
    /// metadata and free of reserved names.
    pub fn normalized_metadata(&self) -> Result<BTreeMap<String, String>> {
        let metadata = normalized_headers(&self.metadata)?;
        for (name, value) in &metadata {
            ascii_metadata(name, value)
                .with_context(|| format!("invalid metadata entry {name}"))?;
        }
        Ok(metadata)
    }

    /// Secret metadata with lowercase names. Errors never contain values.
    pub fn secret_headers(&self) -> Result<BTreeMap<String, SecretString>> {
        let mut result = BTreeMap::new();
        for (name, value) in &self.secret_metadata {
            let lower = name.to_ascii_lowercase();
            ensure!(
                !RESERVED_HEADERS.contains(&lower.as_str()),
                "{lower} is supplied automatically and may not be configured"
            );
            ascii_metadata(&lower, value.expose()).map_err(|_| {
                anyhow::anyhow!("secretMetadata entry {lower} is not valid ASCII gRPC metadata")
            })?;
            ensure!(
                result.insert(lower, value.clone()).is_none(),
                "duplicate secretMetadata name ignoring case"
            );
        }
        Ok(result)
    }

    /// Lowercase names of every template-rendered metadata entry, validated
    /// as gRPC metadata names and free of reserved names.
    fn template_metadata_names(&self) -> Result<std::collections::BTreeSet<String>> {
        let mut names = std::collections::BTreeSet::new();
        let Some(templates) = &self.output_templates else {
            return Ok(names);
        };
        for query in templates
            .default_template
            .iter()
            .chain(templates.routes.values())
        {
            for spec in [&query.added, &query.updated, &query.deleted]
                .into_iter()
                .flatten()
            {
                for name in spec.extension.metadata.keys() {
                    let lower = name.to_ascii_lowercase();
                    let _: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = lower
                        .parse()
                        .with_context(|| format!("invalid template metadata name {name}"))?;
                    ensure!(
                        !RESERVED_HEADERS.contains(&lower.as_str()),
                        "{lower} is supplied automatically and may not be templated"
                    );
                    names.insert(lower);
                }
            }
        }
        Ok(names)
    }

    /// The shared reaction-protocol view used for template validation and
    /// change projection. Connection fields are irrelevant to that use.
    pub fn reaction_config(&self) -> GrpcReactionConfig {
        GrpcReactionConfig {
            endpoint: self
                .destination()
                .map(|destination| destination.uri)
                .unwrap_or_else(|_| self.endpoint.clone()),
            timeout_ms: self.timeout_ms,
            max_retries: self.max_retries,
            metadata: self.metadata.clone().into_iter().collect(),
            batching: self.batching_config(),
            output_format: self.output_format,
            output_templates: self.output_templates.clone(),
            ..GrpcReactionConfig::default()
        }
    }

    pub fn batching_config(&self) -> BatchingConfig {
        self.batching.clone().unwrap_or(BatchingConfig::Fixed {
            batch_size: self.batch_size,
            batch_flush_timeout_ms: self.batch_flush_timeout_ms,
        })
    }
}

/// Checks one gRPC ASCII metadata entry. Binary (`-bin`) names are rejected.
pub fn ascii_metadata(name: &str, value: &str) -> Result<()> {
    let _: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = name.parse()?;
    let _: tonic::metadata::MetadataValue<tonic::metadata::Ascii> = value.parse()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COMPONENT_ID: &str = "63d14bfc-2827-577e-aed6-d4e1f755df25";

    fn outputs() -> Value {
        json!({"q": {"component": COMPONENT_ID, "port": "out"}})
    }

    fn with(extra: Value) -> Value {
        let mut value = json!({"outputs": outputs()});
        if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            value.extend(extra.clone());
        }
        value
    }

    #[test]
    fn stale_producer_keys_and_implicit_ports_are_rejected() {
        for stale in [
            json!({"producer": COMPONENT_ID}),
            json!({"producers": {"q": format!("{COMPONENT_ID}/out")}}),
            json!({"outputs": {"q": {"component": COMPONENT_ID, "stream": "q/out"}}}),
            json!({"outputs": {"bad id": {"component": COMPONENT_ID}}}),
        ] {
            assert!(GrpcSinkConfig::parse(stale.clone()).is_err(), "{stale}");
        }
    }

    #[test]
    fn defaults_serialize_explicitly() -> Result<()> {
        let config = GrpcSinkConfig::parse(with(json!({})))?;
        assert!(GrpcSinkConfig::parse(json!({}))?.outputs.is_empty());
        assert_eq!(
            config.destination()?,
            Destination {
                uri: "http://localhost:50052".into(),
                tls: false
            }
        );
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "outputs": outputs(), "endpoint":"grpc://localhost:50052",
                "metadata":{}, "timeoutMs":60000, "connectTimeoutMs":10000,
                "maxRetries":5, "retryBudgetMs":120000, "failurePolicy":"strict",
                "keepaliveIntervalMs":30000, "keepaliveTimeoutMs":20000,
                "compression":"none", "batchSize":1, "batchFlushTimeoutMs":100,
                "outputFormat":"canonicalJson", "maxBatchBytes":16777216,
                "maxRequestBytes":4194304
            })
        );
        Ok(())
    }

    #[test]
    fn endpoint_schemes_map_to_plaintext_or_tls() -> Result<()> {
        for (endpoint, uri, tls) in [
            ("grpc://h:1", "http://h:1", false),
            ("http://h:1", "http://h:1", false),
            ("grpcs://h:1", "https://h:1", true),
            ("https://h", "https://h", true),
        ] {
            let config = GrpcSinkConfig::parse(with(json!({ "endpoint": endpoint })))?;
            assert_eq!(
                config.destination()?,
                Destination {
                    uri: uri.into(),
                    tls
                }
            );
        }
        Ok(())
    }

    #[test]
    fn secrets_are_redacted_from_debug() -> Result<()> {
        let config = GrpcSinkConfig::parse(with(json!({
            "secretMetadata": {"authorization": "Bearer very-secret-value"}
        })))?;
        assert!(!format!("{config:?}").contains("very-secret-value"));
        assert_eq!(
            config.secret_headers()?["authorization"].expose(),
            "Bearer very-secret-value"
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_reserved_and_removed_settings() {
        for field in [
            json!({"metadata":{"x-drasi-component":"other"}}),
            json!({"metadata":{"x-drasi-port":"other"}}),
            json!({"metadata":{"X-Drasi-Delivery-Id":"other"}}),
            json!({"metadata":{"x-custom":"line\nbreak"}}),
            json!({"metadata":{"custom-bin":"text"}}),
            json!({"metadata":{"a":"1","A":"2"}}),
            json!({"secretMetadata":{"x-drasi-delivery-id":"x"}}),
            json!({"secretMetadata":{"authorization":"line\nbreak"}}),
            json!({"metadata":{"authorization":"a"},"secretMetadata":{"Authorization":"b"}}),
            json!({"secretMetadata":{"x-route":"b"},"outputTemplates":{"defaultTemplate":{"added":{"template":"","metadata":{"x-route":"{{sequence}}"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"template":"","metadata":{"x-drasi-port":"{{sequence}}"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"template":"","metadata":{"bad name":"v"}}}}}),
            json!({"endpoint":"ftp://localhost:50052"}),
            json!({"endpoint":"grpc://localhost:50052/path"}),
            json!({"endpoint":"grpc://user:pw@localhost:50052"}),
            json!({"endpoint":"localhost:50052"}),
            json!({"tlsCaPem":"-----BEGIN CERTIFICATE-----"}),
            json!({"tlsDomainName":"localhost"}),
            json!({"endpoint":"https://h","tlsCaPem":"not pem"}),
            json!({"endpoint":"https://h","tlsClientCertificatePem":"x"}),
            json!({"endpoint":"https://h","tlsDomainName":"bad name"}),
            json!({"batchSize":0}),
            json!({"timeoutMs":0}),
            json!({"connectTimeoutMs":0}),
            json!({"retryBudgetMs":0}),
            json!({"retryBudgetMs":3600001}),
            json!({"maxRetries":101}),
            json!({"keepaliveTimeoutMs":0}),
            json!({"compression":"zstd"}),
            json!({"connectionRetryAttempts":1}),
            json!({"initialConnectionTimeoutMs":1}),
            json!({"unknown":true}),
            json!({"batching":{"mode":"fixed"},"batchSize":2}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"templat":"bad"}}}}),
            json!({"outputTemplates":{"routes":{"q":{"added":{"url":"not-a-grpc-option"}}}}}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"template":"{{#if}}"}}}}),
        ] {
            let value = with(field);
            assert!(GrpcSinkConfig::parse(value.clone()).is_err(), "{value}");
        }
    }
}
