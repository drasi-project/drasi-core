// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Protocol-independent settings and validation shared by network components.

use anyhow::{ensure, Result};
#[cfg(feature = "ingress")]
use drasi_computation_contracts::{ComponentId, StreamId};
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use serde_json::Value;
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use std::collections::BTreeMap;
use std::time::Duration;

pub const MAX_INGRESS_CAPACITY: usize = 1_048_576;
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RETRIES: u32 = 100;

pub fn duration(value: u64, field: &str) -> Result<Duration> {
    let value = Duration::from_millis(value);
    ensure!(
        !value.is_zero() && std::time::Instant::now().checked_add(value).is_some(),
        "{field} must be positive and representable"
    );
    Ok(value)
}

pub fn timeout_ms() -> u64 {
    60_000
}

#[cfg(feature = "ingress")]
pub fn host() -> String {
    "0.0.0.0".into()
}

#[cfg(feature = "ingress")]
pub fn source_id() -> String {
    "facilities-db".into()
}

#[cfg(feature = "ingress")]
pub fn ingress_capacity() -> usize {
    1024
}

/// Validated connection and admission settings, without protocol-specific options.
#[cfg(feature = "ingress")]
#[derive(Clone, Debug)]
pub struct IngressConfig {
    pub stream: StreamId,
    pub source_id: String,
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
    pub ingress_capacity: usize,
    pub max_message_bytes: usize,
}

#[cfg(feature = "ingress")]
impl IngressConfig {
    pub fn validate(&self) -> Result<()> {
        ComponentId::try_new(self.source_id.as_str())?;
        ensure!(
            !self.source_id.contains('/'),
            "sourceId must be one URL path segment"
        );
        self.host.parse::<std::net::IpAddr>()?;
        duration(self.timeout_ms, "timeoutMs")?;
        ensure!(
            (1..=MAX_INGRESS_CAPACITY).contains(&self.ingress_capacity),
            "ingressCapacity must be in 1..=1048576"
        );
        ensure!(
            (1..=MAX_MESSAGE_BYTES).contains(&self.max_message_bytes),
            "maxMessageBytes must be in 1..=33554432"
        );
        Ok(())
    }
}

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailurePolicy {
    #[default]
    Strict,
    Skip,
}

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
impl FailurePolicy {
    pub fn delivery(
        self,
        result: Result<()>,
        component: &str,
        output: &str,
        sequence: u64,
    ) -> Result<()> {
        match (self, result) {
            (_, Ok(())) => Ok(()),
            (Self::Strict, Err(error)) => Err(error),
            (Self::Skip, Err(error)) => {
                eprintln!("drasi.network deliberate delivery skip: component={component} output={output} sequence={sequence} error={error:#}");
                Ok(())
            }
        }
    }
}

/// Header and gRPC metadata names carrying the real component and output port
/// that produced each request's data.
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub const COMPONENT_HEADER: &str = "x-drasi-component";
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub const PORT_HEADER: &str = "x-drasi-port";

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub fn validate_output_templates(value: &Value, extensions: &[&str]) -> Result<()> {
    let Some(templates) = value.get("outputTemplates") else {
        return Ok(());
    };
    fn fields<'a>(
        value: &'a Value,
        allowed: &[&str],
    ) -> Result<&'a serde_json::Map<String, Value>> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("output template must be an object"))?;
        for field in object.keys() {
            ensure!(
                allowed.contains(&field.as_str()),
                "unknown output template field {field}"
            );
        }
        Ok(object)
    }
    let templates = fields(templates, &["routes", "defaultTemplate"])?;
    let mut queries = Vec::new();
    if let Some(default) = templates.get("defaultTemplate") {
        queries.push(default);
    }
    if let Some(routes) = templates.get("routes") {
        queries.extend(
            routes
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("output template routes must be an object"))?
                .values(),
        );
    }
    let mut options = vec!["template"];
    options.extend_from_slice(extensions);
    for query in queries {
        for spec in fields(query, &["added", "updated", "deleted"])?.values() {
            if !spec.is_null() {
                fields(spec, &options)?;
            }
        }
    }
    Ok(())
}

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub fn normalized_headers(headers: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (key, value) in headers {
        let name: reqwest::header::HeaderName = key.parse()?;
        let _: reqwest::header::HeaderValue = value.parse()?;
        ensure!(
            name.as_str() != COMPONENT_HEADER && name.as_str() != PORT_HEADER,
            "{name} identifies the producing output and is supplied automatically"
        );
        ensure!(
            result.insert(name.to_string(), value.clone()).is_none(),
            "duplicate header name ignoring case"
        );
    }
    Ok(result)
}

#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub async fn backoff(attempt: u32) {
    tokio::time::sleep(Duration::from_millis(
        100u64.saturating_mul(1u64 << attempt.min(6)).min(5000),
    ))
    .await;
}
