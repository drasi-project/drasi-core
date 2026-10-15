// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{ensure, Result};
use drasi_computation_network_common::config::{self as shared, IngressConfig};
use drasi_lib::computation::v1::StreamId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

fn port() -> u16 {
    9000
}

fn message_bytes() -> usize {
    2 * 1024 * 1024
}
fn batch_events() -> Option<usize> {
    Some(1024)
}

/// Settings for one native HTTP source, including its HTTP batch limit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSourceConfig {
    pub stream: StreamId,
    #[serde(default = "shared::source_id")]
    pub source_id: String,
    #[serde(default = "shared::host")]
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    #[serde(default = "shared::timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "shared::ingress_capacity")]
    pub ingress_capacity: usize,
    #[serde(default = "message_bytes")]
    pub max_message_bytes: usize,
    #[serde(default = "batch_events", skip_serializing_if = "Option::is_none")]
    pub max_batch_events: Option<usize>,
    #[serde(default)]
    pub adaptive_enabled: bool,
}

impl HttpSourceConfig {
    pub fn from_value(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.ingress().validate()?;
        ensure!(
            !config.adaptive_enabled,
            "adaptive source processing is unsupported"
        );
        ensure!(
            config
                .max_batch_events
                .is_some_and(|value| (1..=1_048_576).contains(&value)),
            "maxBatchEvents must be in 1..=1048576"
        );
        Ok(config)
    }

    /// Compatibility entry point; HTTP settings no longer select a protocol.
    #[deprecated(note = "use HttpSourceConfig::from_value without a protocol flag")]
    pub fn parse(value: Value, grpc: bool) -> Result<Self> {
        ensure!(!grpc, "HTTP source configuration cannot select gRPC");
        Self::from_value(value)
    }

    pub(crate) fn ingress(&self) -> IngressConfig {
        IngressConfig {
            stream: self.stream.clone(),
            source_id: self.source_id.clone(),
            host: self.host.clone(),
            port: self.port,
            timeout_ms: self.timeout_ms,
            ingress_capacity: self.ingress_capacity,
            max_message_bytes: self.max_message_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_and_serialized_settings_remain_compatible() -> Result<()> {
        let config = HttpSourceConfig::from_value(json!({"stream":"events/out"}))?;
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "stream":"events/out", "sourceId":"facilities-db", "host":"0.0.0.0",
                "port":9000, "timeoutMs":60000, "ingressCapacity":1024,
                "maxMessageBytes":2097152, "maxBatchEvents":1024, "adaptiveEnabled":false
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_shared_and_http_only_settings() {
        for field in [
            json!({"adaptiveEnabled":true}),
            json!({"maxBatchEvents":0}),
            json!({"maxBatchEvents":null}),
            json!({"maxMessageBytes":0}),
            json!({"ingressCapacity":0}),
            json!({"timeoutMs":0}),
            json!({"sourceId":"nested/source"}),
            json!({"unknown":true}),
        ] {
            let mut value = json!({"stream":"events/out"});
            value
                .as_object_mut()
                .expect("object")
                .extend(field.as_object().expect("object").clone());
            assert!(
                HttpSourceConfig::from_value(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn compatibility_parser_does_not_switch_component_protocol() -> Result<()> {
        assert_eq!(
            HttpSourceConfig::parse(json!({"stream":"s"}), false)?.port,
            9000
        );
        assert!(HttpSourceConfig::parse(json!({"stream":"s"}), true).is_err());
        Ok(())
    }
}
