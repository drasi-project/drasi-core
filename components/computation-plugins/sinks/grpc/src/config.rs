// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
pub use drasi_computation_network_common::config::{backoff, duration, FailurePolicy};
use drasi_computation_network_common::config::{input_stream, normalized_headers, timeout_ms};
use drasi_lib::computation::v1::StreamId;
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

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    #[default]
    CanonicalJson,
    Proto,
}

/// Settings and validation for the native gRPC query-output sink.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcSinkConfig {
    pub query_id: String,
    pub stream: StreamId,
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
    #[serde(default)]
    pub output_format: OutputFormat,
}

impl GrpcSinkConfig {
    pub fn parse(mut value: Value) -> Result<Self> {
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
            config.max_retries <= 100 && (1..=100).contains(&config.connection_retry_attempts),
            "retry attempts exceed supported bounds"
        );
        ensure!(config.batch_size == 1, "native gRPC supports batchSize=1 only; fixed multi-item/adaptive batching is unsupported");
        for (key, value) in &config.metadata {
            let _: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = key.parse()?;
            let _: tonic::metadata::MetadataValue<tonic::metadata::Ascii> = value.parse()?;
        }
        config.metadata = normalized_headers(&config.metadata, &config.query_id)?;
        Ok(config)
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
                "batchFlushTimeoutMs":100, "outputFormat":"canonicalJson"
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
            json!({"batchSize":2}),
            json!({"connectionRetryAttempts":0}),
            json!({"initialConnectionTimeoutMs":0}),
            json!({"unknown":true}),
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
