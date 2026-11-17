// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration for `drasi.network/grpc-source`.

use anyhow::{ensure, Result};
use drasi_computation_contracts::StreamId;
use drasi_computation_network_common::config::{
    self as shared, IngressConfig, SecretString, SecuritySettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Upper bound on concurrently open streaming RPCs.
pub const MAX_CONCURRENT_STREAMS: usize = 65_536;

fn port() -> u16 {
    50051
}
fn message_bytes() -> usize {
    4 * 1024 * 1024
}
fn max_concurrent_streams() -> usize {
    64
}
fn stream_idle_timeout_ms() -> u64 {
    300_000
}
fn header_read_timeout_ms() -> u64 {
    10_000
}

/// Settings for one native gRPC source.
///
/// Secret fields (`authToken`, `authTokenSecondary`, `tlsPrivateKeyPem`) are
/// supplied as host secret references and are never printed by `Debug`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrpcSourceConfig {
    /// Output stream identifier.
    pub stream: StreamId,
    /// Literal IP address to listen on.
    #[serde(default = "shared::host")]
    pub host: String,
    /// TCP port; zero requests an available port.
    #[serde(default = "port")]
    pub port: u16,
    /// Unary request and admission deadline in milliseconds.
    #[serde(default = "shared::timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum simultaneous unary admissions; also the volatile queue bound.
    #[serde(default = "shared::ingress_capacity")]
    pub ingress_capacity: usize,
    /// Maximum decoded message size in bytes.
    #[serde(default = "message_bytes")]
    pub max_message_bytes: usize,
    /// Maximum simultaneously open client connections.
    #[serde(default = "shared::max_connections")]
    pub max_connections: usize,
    /// Maximum simultaneously open streaming RPCs, separate from unary slots.
    #[serde(default = "max_concurrent_streams")]
    pub max_concurrent_streams: usize,
    /// A streaming RPC that sends nothing for this long is closed.
    #[serde(default = "stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    /// Deadline for a client to finish the TLS handshake and send the HTTP/2
    /// connection preface; named and bounded like the HTTP source's setting.
    #[serde(default = "header_read_timeout_ms")]
    pub header_read_timeout_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_secondary: Option<SecretString>,
    #[serde(default)]
    pub allow_unauthenticated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_per_second: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_burst: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_certificate_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_private_key_pem: Option<SecretString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_ca_pem: Option<String>,
}

impl GrpcSourceConfig {
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        let mut fields = shared::ingress_options(
            port(),
            message_bytes(),
            "Maximum decoded gRPC message size in bytes.",
        );
        fields.extend([
            Field::new("maxConcurrentStreams", Integer, false, "Maximum simultaneously open streaming RPCs (StreamEvents), separate from unary request slots.").default(max_concurrent_streams()).minimum(1).maximum(MAX_CONCURRENT_STREAMS as u64),
            Field::new("streamIdleTimeoutMs", Integer, false, "A streaming RPC that sends no message for this many milliseconds is closed.").default(stream_idle_timeout_ms()).minimum(1),
            Field::new("headerReadTimeoutMs", Integer, false, "Deadline in milliseconds for a client to finish the TLS handshake and send the HTTP/2 connection preface; bounds idle connections holding maxConnections slots.").default(header_read_timeout_ms()).minimum(1).maximum(600_000),
        ]);
        ConfigDefinition::try_new(fields)
            .unwrap_or_else(|error| unreachable!("constant gRPC source configuration: {error}"))
    }

    pub fn from_value(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.ingress()?.validate()?;
        ensure!(
            (1..=MAX_CONCURRENT_STREAMS).contains(&self.max_concurrent_streams),
            "maxConcurrentStreams must be in 1..=65536"
        );
        shared::duration(self.stream_idle_timeout_ms, "streamIdleTimeoutMs")?;
        ensure!(
            (1..=600_000).contains(&self.header_read_timeout_ms),
            "headerReadTimeoutMs must be in 1..=600000"
        );
        Ok(())
    }

    pub fn ingress(&self) -> Result<IngressConfig> {
        let security = SecuritySettings {
            auth_token: self.auth_token.as_ref(),
            auth_token_secondary: self.auth_token_secondary.as_ref(),
            allow_unauthenticated: self.allow_unauthenticated,
            rate_limit_per_second: self.rate_limit_per_second,
            rate_limit_burst: self.rate_limit_burst,
            tls_certificate_pem: self.tls_certificate_pem.as_deref(),
            tls_private_key_pem: self.tls_private_key_pem.as_ref(),
            tls_client_ca_pem: self.tls_client_ca_pem.as_deref(),
        }
        .build()?;
        Ok(IngressConfig {
            stream: self.stream.clone(),
            host: self.host.clone(),
            port: self.port,
            timeout_ms: self.timeout_ms,
            ingress_capacity: self.ingress_capacity,
            max_message_bytes: self.max_message_bytes,
            max_connections: self.max_connections,
            security,
        })
    }

    /// Refuses an unauthenticated listener on a non-loopback address. Every
    /// gRPC service admits data or producer metadata, so all are token-protected.
    pub fn validate_exposure(&self) -> Result<()> {
        self.ingress()?.validate_exposure(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_serialize_explicitly_and_bind_loopback() -> Result<()> {
        let config = GrpcSourceConfig::from_value(json!({"stream":"events/out"}))?;
        assert_eq!(
            serde_json::to_value(config)?,
            json!({
                "stream":"events/out", "host":"127.0.0.1",
                "port":50051, "timeoutMs":60000, "ingressCapacity":1024,
                "maxMessageBytes":4194304, "maxConnections":1024,
                "maxConcurrentStreams":64, "streamIdleTimeoutMs":300000,
                "headerReadTimeoutMs":10000, "allowUnauthenticated":false
            })
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_and_removed_settings() {
        for field in [
            json!({"maxBatchEvents":1}),
            json!({"adaptiveEnabled":false}),
            json!({"maxMessageBytes":33554433}),
            json!({"ingressCapacity":0}),
            json!({"maxConcurrentStreams":0}),
            json!({"streamIdleTimeoutMs":0}),
            json!({"headerReadTimeoutMs":0}),
            json!({"headerReadTimeoutMs":600001}),
            json!({"timeoutMs":0}),
            json!({"host":"localhost"}),
            json!({"authToken":"short"}),
            json!({"sourceId":"nested/source"}),
            json!({"unknown":true}),
        ] {
            let mut value = json!({"stream":"events/out"});
            value
                .as_object_mut()
                .expect("object")
                .extend(field.as_object().expect("object").clone());
            assert!(
                GrpcSourceConfig::from_value(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn non_loopback_exposure_requires_authentication() -> Result<()> {
        assert!(
            GrpcSourceConfig::from_value(json!({"stream":"s","host":"0.0.0.0"}))?
                .validate_exposure()
                .is_err()
        );
        GrpcSourceConfig::from_value(
            json!({"stream":"s","host":"0.0.0.0","authToken":"0123456789abcdef"}),
        )?
        .validate_exposure()?;
        Ok(())
    }
}
