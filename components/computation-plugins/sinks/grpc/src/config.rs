// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
use drasi_computation_contracts::StreamId;
pub use drasi_computation_network_common::config::{backoff, duration, FailurePolicy};
use drasi_computation_network_common::config::{input_stream, normalized_headers, timeout_ms};
pub use drasi_grpc_common::output::config::{
    BatchingConfig, GrpcReactionConfig, OutputFormat, OutputTemplates,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

fn endpoint() -> String {
    "grpc://localhost:50052".into()
}

fn retries() -> u32 {
    5
}
fn attempts() -> u32 {
    10
}
fn connect_timeout() -> u64 {
    15_000
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

/// Settings and validation for the native gRPC query-output sink.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcSinkConfig {
    pub query_id: String,
    pub stream: StreamId,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query_streams: BTreeMap<String, StreamId>,
    #[serde(default = "endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    #[serde(default = "attempts")]
    pub connection_retry_attempts: u32,
    #[serde(default = "connect_timeout")]
    pub initial_connection_timeout_ms: u64,
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

impl GrpcSinkConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        use serde_json::json;
        ConfigDefinition::try_new([
            Field::new("queryId", String, false, "Query whose result changes this sink accepts; required unless queryStreams is supplied."),
            Field::new("stream", String, false, "Input stream identifier.").default_description("queryId followed by /out."),
            Field::new("queryStreams", Object, false, "Map of query identifiers to their connected streams."),
            Field::new("endpoint", String, false, "Plaintext grpc:// or http:// destination; TLS endpoints are not supported.").default(endpoint()),
            Field::new("metadata", Object, false, "ASCII gRPC request metadata; the query identity header is supplied automatically.").default(json!({})),
            Field::new("timeoutMs", Integer, false, "gRPC request timeout in milliseconds; must be representable by the system clock.").default(timeout_ms()).minimum(1),
            Field::new("maxRetries", Integer, false, "Additional delivery attempts after the first attempt.").default(retries()).minimum(0).maximum(drasi_computation_network_common::config::MAX_RETRIES as u64),
            Field::new("failurePolicy", String, false, "strict reports delivery failure; skip deliberately discards failed output with an error report.").default(json!(FailurePolicy::default())).allowed([json!(FailurePolicy::Strict), json!(FailurePolicy::Skip)]),
            Field::new("connectionRetryAttempts", Integer, false, "Total connection attempts during startup, including the first.").default(attempts()).minimum(1).maximum(drasi_computation_network_common::config::MAX_RETRIES as u64),
            Field::new("initialConnectionTimeoutMs", Integer, false, "Timeout for each startup connection attempt, in milliseconds.").default(connect_timeout()).minimum(1),
            Field::new("batchSize", Integer, false, "Fixed batch item target; may not be combined with batching.").default(batch_size()).minimum(1).maximum(65536),
            Field::new("batchFlushTimeoutMs", Integer, false, "Host collection deadline in fixed mode; sparse batches flush without acknowledging early.").default(flush_timeout()).minimum(1).maximum(60000),
            Field::new("batching", Object, false, "Legacy fixed or adaptive batching configuration; mutually exclusive with flat batchSize/batchFlushTimeoutMs."),
            Field::new("outputTemplates", Object, false, "Legacy per-query/default body and request-metadata templates; typed row images remain unchanged."),
            Field::new("maxBatchBytes", Integer, false, "Host collection byte bound; larger individual inputs use direct handling.").default(batch_bytes()).minimum(1).maximum(64 * 1024 * 1024),
            Field::new("maxRequestBytes", Integer, false, "Maximum encoded protobuf request; compatible item groups split before this limit.").default(request_bytes()).minimum(1).maximum(32 * 1024 * 1024),
            Field::new("outputFormat", String, false, "canonicalJson includes the canonical JSON payload; proto sends the typed before/after fields without that payload.").default(json!(OutputFormat::default())).allowed([json!(OutputFormat::CanonicalJson), json!(OutputFormat::Proto)]),
        ]).expect("constant gRPC sink configuration")
    }

    pub fn parse(mut value: Value) -> Result<Self> {
        drasi_computation_network_common::config::validate_output_templates(&value, &["metadata"])?;
        ensure!(
            value.get("batching").is_none()
                || (value.get("batchSize").is_none() && value.get("batchFlushTimeoutMs").is_none()),
            "batching cannot be combined with flat batch settings"
        );
        input_stream(&mut value)?;
        let mut config: Self = serde_json::from_value(value)?;
        let url = reqwest::Url::parse(&config.connection_uri())?;
        ensure!(
            url.scheme() == "http"
                && url.host_str().is_some()
                && url.path() == "/"
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "only plaintext grpc:// or http:// endpoints are supported"
        );
        duration(config.timeout_ms, "timeoutMs")?;
        duration(
            config.initial_connection_timeout_ms,
            "initialConnectionTimeoutMs",
        )?;
        duration(config.batch_flush_timeout_ms, "batchFlushTimeoutMs")?;
        ensure!(
            config.max_retries <= drasi_computation_network_common::config::MAX_RETRIES
                && (1..=drasi_computation_network_common::config::MAX_RETRIES)
                    .contains(&config.connection_retry_attempts),
            "retry attempts exceed supported bounds"
        );
        ensure!(
            (1..=65536).contains(&config.batch_size) && config.batch_flush_timeout_ms <= 60_000,
            "invalid fixed batching bounds"
        );
        ensure!(
            (1..=64 * 1024 * 1024).contains(&config.max_batch_bytes),
            "invalid maxBatchBytes"
        );
        ensure!(
            (1..=32 * 1024 * 1024).contains(&config.max_request_bytes),
            "invalid maxRequestBytes"
        );
        for (key, value) in &config.metadata {
            let _: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = key.parse()?;
            let _: tonic::metadata::MetadataValue<tonic::metadata::Ascii> = value.parse()?;
        }
        config.metadata = normalized_headers(&config.metadata, &config.query_id)?;
        let legacy = config.reaction_config();
        let queries = if config.query_streams.is_empty() {
            vec![config.query_id.clone()]
        } else {
            config.query_streams.keys().cloned().collect()
        };
        legacy.validate(&queries)?;
        match legacy.batching {
            BatchingConfig::Fixed {
                batch_size,
                batch_flush_timeout_ms,
            } => ensure!(
                batch_size <= 65_536 && batch_flush_timeout_ms <= 60_000,
                "fixed batching bounds exceeded"
            ),
            BatchingConfig::Adaptive {
                adaptive_max_batch_size,
                adaptive_window_size,
                adaptive_batch_timeout_ms,
                ..
            } => ensure!(
                adaptive_max_batch_size <= 65_536
                    && adaptive_window_size <= 255
                    && adaptive_batch_timeout_ms <= 60_000,
                "adaptive batching bounds exceeded"
            ),
        }
        Ok(config)
    }

    pub fn reaction_config(&self) -> GrpcReactionConfig {
        GrpcReactionConfig {
            endpoint: self.endpoint.clone(),
            timeout_ms: self.timeout_ms,
            max_retries: self.max_retries,
            connection_retry_attempts: self.connection_retry_attempts,
            initial_connection_timeout_ms: self.initial_connection_timeout_ms,
            metadata: self.metadata.clone().into_iter().collect(),
            batching: self.batching.clone().unwrap_or(BatchingConfig::Fixed {
                batch_size: self.batch_size,
                batch_flush_timeout_ms: self.batch_flush_timeout_ms,
            }),
            output_format: self.output_format,
            output_templates: self.output_templates.clone(),
        }
    }

    pub fn connection_uri(&self) -> String {
        self.endpoint
            .strip_prefix("grpc://")
            .map(|suffix| format!("http://{suffix}"))
            .unwrap_or_else(|| self.endpoint.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_metadata_remain_compatible() -> Result<()> {
        let config = GrpcSinkConfig::parse(json!({"queryId":"q"}))?;
        assert_eq!(config.connection_uri(), "http://localhost:50052");
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "queryId":"q", "stream":"q/out", "endpoint":"grpc://localhost:50052",
                "metadata":{"x-query-sequence":"q"}, "timeoutMs":60000, "maxRetries":5,
                "failurePolicy":"strict", "connectionRetryAttempts":10,
                "initialConnectionTimeoutMs":15000, "batchSize":1,
                "batchFlushTimeoutMs":100, "outputFormat":"canonicalJson",
                "maxBatchBytes":16777216, "maxRequestBytes":4194304
            })
        );
        Ok(())
    }

    #[test]
    fn validates_grpc_metadata_and_options_in_this_component() {
        for field in [
            json!({"metadata":{"x-query-sequence":"other"}}),
            json!({"metadata":{"x-custom":"line\nbreak"}}),
            json!({"metadata":{"custom-bin":"text"}}),
            json!({"endpoint":"https://localhost:50052"}),
            json!({"batchSize":0}),
            json!({"connectionRetryAttempts":0}),
            json!({"initialConnectionTimeoutMs":0}),
            json!({"unknown":true}),
            json!({"outputTemplates":{"defaultTemplate":{"added":{"templat":"bad"}}}}),
            json!({"outputTemplates":{"routes":{"q":{"added":{"url":"not-a-grpc-option"}}}}}),
        ] {
            let mut value = json!({"queryId":"q"});
            value
                .as_object_mut()
                .expect("object")
                .extend(field.as_object().expect("object").clone());
            assert!(GrpcSinkConfig::parse(value.clone()).is_err(), "{value}");
        }
    }
}
