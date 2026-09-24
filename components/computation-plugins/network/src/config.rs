// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
use drasi_lib::computation::v1::{ComponentId, StreamId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, net::IpAddr, time::Duration};

fn host() -> String {
    "0.0.0.0".into()
}
fn source_id() -> String {
    "facilities-db".into()
}
fn timeout() -> u64 {
    60_000
}
fn ingress() -> usize {
    1024
}
fn retries() -> u32 {
    3
}
fn grpc_retries() -> u32 {
    5
}
fn endpoint() -> String {
    "grpc://localhost:50052".into()
}
fn one() -> usize {
    1
}
fn flush() -> u64 {
    100
}
fn attempts() -> u32 {
    10
}
fn connect_timeout() -> u64 {
    15_000
}

pub(crate) fn duration(value: u64, field: &str) -> Result<Duration> {
    let value = Duration::from_millis(value);
    ensure!(
        !value.is_zero() && std::time::Instant::now().checked_add(value).is_some(),
        "{field} must be positive and representable"
    );
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceConfig {
    pub stream: StreamId,
    #[serde(default = "source_id")]
    pub source_id: String,
    #[serde(default = "host")]
    pub host: String,
    pub port: u16,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "ingress")]
    pub ingress_capacity: usize,
    pub max_message_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_batch_events: Option<usize>,
    #[serde(default)]
    pub adaptive_enabled: bool,
}
impl SourceConfig {
    pub fn parse(mut value: Value, grpc: bool) -> Result<Self> {
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("source configuration must be an object"))?;
        object
            .entry("port")
            .or_insert(Value::from(if grpc { 50051 } else { 9000 }));
        object
            .entry("maxMessageBytes")
            .or_insert(Value::from(if grpc {
                4 * 1024 * 1024
            } else {
                2 * 1024 * 1024
            }));
        if !grpc {
            object.entry("maxBatchEvents").or_insert(Value::from(1024));
        }
        let config: Self = serde_json::from_value(value)?;
        ComponentId::try_new(config.source_id.as_str())?;
        ensure!(
            !config.source_id.contains('/'),
            "sourceId must be one URL path segment"
        );
        config.host.parse::<IpAddr>()?;
        duration(config.timeout_ms, "timeoutMs")?;
        ensure!(
            (1..=1_048_576).contains(&config.ingress_capacity),
            "ingressCapacity must be in 1..=1048576"
        );
        ensure!(
            (1..=32 * 1024 * 1024).contains(&config.max_message_bytes),
            "maxMessageBytes must be in 1..=33554432"
        );
        ensure!(
            !config.adaptive_enabled,
            "adaptive source processing is unsupported"
        );
        if grpc {
            ensure!(
                config.max_batch_events.is_none(),
                "gRPC source does not accept HTTP maxBatchEvents"
            );
        } else {
            ensure!(
                config
                    .max_batch_events
                    .is_some_and(|value| (1..=1_048_576).contains(&value)),
                "maxBatchEvents must be in 1..=1048576"
            );
        }
        Ok(config)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailurePolicy {
    #[default]
    Strict,
    Skip,
}
impl FailurePolicy {
    pub(crate) fn delivery(
        self,
        result: Result<()>,
        component: &str,
        query: &str,
        sequence: u64,
    ) -> Result<()> {
        match (self, result) {
            (_, Ok(())) => Ok(()),
            (Self::Strict, Err(error)) => Err(error),
            (Self::Skip, Err(error)) => {
                // Native plugins have a separate logger registry. stderr is an
                // observable report even when no plugin logger is installed.
                eprintln!("drasi.network deliberate delivery skip: component={component} query={query} sequence={sequence} error={error:#}");
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSinkConfig {
    pub query_id: String,
    pub stream: StreamId,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub failure_policy: FailurePolicy,
}

fn input_stream(value: &mut Value) -> Result<()> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("sink configuration must be an object"))?;
    let query_id = object
        .get("queryId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("queryId is required"))?;
    ComponentId::try_new(query_id)?;
    let stream = format!("{query_id}/out");
    object.entry("stream").or_insert(Value::String(stream));
    Ok(())
}
fn normalized_headers(
    headers: &BTreeMap<String, String>,
    query: &str,
    grpc: bool,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (key, value) in headers {
        let name: reqwest::header::HeaderName = key.parse()?;
        let _: reqwest::header::HeaderValue = value.parse()?;
        ensure!(
            result.insert(name.to_string(), value.clone()).is_none(),
            "duplicate header name ignoring case"
        );
        if grpc {
            let _: tonic::metadata::MetadataKey<tonic::metadata::Ascii> = key.parse()?;
            let _: tonic::metadata::MetadataValue<tonic::metadata::Ascii> = value.parse()?;
        }
    }
    if let Some(value) = result.get("x-query-sequence") {
        ensure!(value == query, "x-query-sequence must match queryId");
    }
    result.insert("x-query-sequence".into(), query.into());
    if !grpc {
        if let Some(value) = result.get("content-type") {
            ensure!(
                value == "application/json",
                "native HTTP output is application/json"
            );
        }
        result.insert("content-type".into(), "application/json".into());
    }
    Ok(result)
}
impl HttpSinkConfig {
    pub fn parse(mut value: Value) -> Result<Self> {
        input_stream(&mut value)?;
        let mut config: Self = serde_json::from_value(value)?;
        let url = reqwest::Url::parse(&config.url)?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "unsupported HTTP URL"
        );
        duration(config.timeout_ms, "timeoutMs")?;
        ensure!(config.max_retries <= 100, "maxRetries must not exceed 100");
        config.headers = normalized_headers(&config.headers, &config.query_id, false)?;
        Ok(config)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputFormat {
    #[default]
    CanonicalJson,
    Proto,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcSinkConfig {
    pub query_id: String,
    pub stream: StreamId,
    #[serde(default = "endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "grpc_retries")]
    pub max_retries: u32,
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    #[serde(default = "attempts")]
    pub connection_retry_attempts: u32,
    #[serde(default = "connect_timeout")]
    pub initial_connection_timeout_ms: u64,
    #[serde(default = "one")]
    pub batch_size: usize,
    #[serde(default = "flush")]
    pub batch_flush_timeout_ms: u64,
    #[serde(default)]
    pub output_format: OutputFormat,
}
impl GrpcSinkConfig {
    pub fn parse(mut value: Value) -> Result<Self> {
        input_stream(&mut value)?;
        let mut config: Self = serde_json::from_value(value)?;
        let normalized = config.connection_uri();
        let url = reqwest::Url::parse(&normalized)?;
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
        config.metadata = normalized_headers(&config.metadata, &config.query_id, true)?;
        Ok(config)
    }
    pub(crate) fn connection_uri(&self) -> String {
        self.endpoint
            .strip_prefix("grpc://")
            .map(|suffix| format!("http://{suffix}"))
            .unwrap_or_else(|| self.endpoint.clone())
    }
}

pub(crate) async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(
        100u64.saturating_mul(1u64 << attempt.min(6)).min(5000),
    ))
    .await;
}
