// Copyright 2025 The Drasi Authors.
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

//! Configuration types for the OpenTelemetry source.
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: otel
//! properties:
//!   grpcBind: "0.0.0.0:4317"
//!   httpBind: "0.0.0.0:4318"
//!   metricAllowlist: ["latency_p99_ms"]
//!   heartbeatMetric: "health.heartbeat"
//!   dependencyTtlSecs: 300
//!   logEventTtlSecs: 60
//!   rejectDerived: true
//! ```

use anyhow::Context;
use serde::{Deserialize, Serialize};

use drasi_lib::DurabilityConfig;

/// Default OTLP/gRPC bind address (standard collector port).
pub fn default_grpc_bind() -> String {
    "0.0.0.0:4317".to_string()
}

pub fn default_destination_attributes() -> Vec<String> {
    vec!["peer.service".to_string()]
}

pub fn default_span_kinds() -> Vec<String> {
    vec!["CLIENT".to_string()]
}

pub fn default_log_min_severity() -> String {
    "ERROR".to_string()
}

pub fn default_log_event_ttl_secs() -> u64 {
    60
}

pub fn default_dependency_ttl_secs() -> u64 {
    300
}

pub fn default_max_services() -> usize {
    1000
}

pub fn default_max_metrics() -> usize {
    2000
}

pub fn default_max_dependencies() -> usize {
    5000
}

pub fn default_max_log_events() -> usize {
    5000
}

pub fn default_reject_derived() -> bool {
    true
}

pub fn default_max_request_bytes() -> usize {
    4 * 1024 * 1024
}

/// OpenTelemetry source configuration.
///
/// Configures inbound OTLP listeners, admission filters, TTL, and optional WAL
/// durability. Call [`OtelSourceConfig::validate`] before use; [`OtelSourceBuilder::build`](crate::OtelSourceBuilder::build)
/// does this automatically.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct OtelSourceConfig {
    /// OTLP/gRPC listen address (`host:port`). Empty disables gRPC.
    #[serde(default = "default_grpc_bind")]
    pub grpc_bind: String,

    /// Optional OTLP/HTTP protobuf listen address (`host:port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_bind: Option<String>,

    /// PEM server certificate path. Unset = plaintext (local-demo exception).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,

    /// PEM server private key path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,

    /// Optional client CA for mTLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_ca_path: Option<String>,

    /// Static bearer token. Identity provider Token credentials take precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,

    /// Accepted metric names. Empty rejects every metric. `*` allows all.
    /// Only `*` wildcards are supported (`latency_*`, `*_p99`); `?` and `**` are not.
    #[serde(default)]
    pub metric_allowlist: Vec<String>,

    /// Extra data-point attributes that extend metric identity.
    /// Unlisted attributes are ignored and do not change the Metric id.
    #[serde(default)]
    pub metric_identity_attributes: Vec<String>,

    /// Span attributes tried in order for the destination service name.
    #[serde(default = "default_destination_attributes")]
    pub destination_attributes: Vec<String>,

    /// Accepted span kinds (`CLIENT`, `SERVER`, `PRODUCER`, `CONSUMER`, `INTERNAL`).
    #[serde(default = "default_span_kinds")]
    pub span_kinds: Vec<String>,

    /// Metric name that refreshes a service Heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_metric: Option<String>,

    /// Log `event_name` that refreshes a service Heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_event_name: Option<String>,

    /// Minimum log severity for LogEvent admission (`INFO`, `WARN`, `ERROR`, …).
    #[serde(default = "default_log_min_severity")]
    pub log_min_severity: String,

    /// If non-empty, only these log `event_name` values become LogEvent nodes.
    /// Same `*` glob rules as `metric_allowlist`; `?` and `**` are not supported.
    #[serde(default)]
    pub log_event_name_allowlist: Vec<String>,

    /// How long LogEvent nodes live before the sweeper deletes them.
    #[serde(default = "default_log_event_ttl_secs")]
    pub log_event_ttl_secs: u64,

    /// How long a DEPENDS_ON edge lives without a refreshing client span.
    #[serde(default = "default_dependency_ttl_secs")]
    pub dependency_ttl_secs: u64,

    /// Maximum distinct Service nodes.
    #[serde(default = "default_max_services")]
    pub max_services: usize,

    /// Maximum distinct Metric nodes.
    #[serde(default = "default_max_metrics")]
    pub max_metrics: usize,

    /// Maximum DEPENDS_ON edges.
    #[serde(default = "default_max_dependencies")]
    pub max_dependencies: usize,

    /// Maximum live LogEvent nodes.
    #[serde(default = "default_max_log_events")]
    pub max_log_events: usize,

    /// Drop records whose `drasi.source.origin` is `derived`.
    #[serde(default = "default_reject_derived")]
    pub reject_derived: bool,

    /// Maximum decoded OTLP request size in bytes for gRPC and HTTP. Default: 4 MiB.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,

    /// Optional WAL durability for projected SourceChanges. Default: off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<DurabilityConfig>,
}

impl std::fmt::Debug for OtelSourceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtelSourceConfig")
            .field("grpc_bind", &self.grpc_bind)
            .field("http_bind", &self.http_bind)
            .field("tls_cert_path", &self.tls_cert_path)
            .field("tls_key_path", &self.tls_key_path)
            .field("tls_client_ca_path", &self.tls_client_ca_path)
            .field(
                "auth_token",
                &self.auth_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("metric_allowlist", &self.metric_allowlist)
            .field(
                "metric_identity_attributes",
                &self.metric_identity_attributes,
            )
            .field("destination_attributes", &self.destination_attributes)
            .field("span_kinds", &self.span_kinds)
            .field("heartbeat_metric", &self.heartbeat_metric)
            .field("heartbeat_event_name", &self.heartbeat_event_name)
            .field("log_min_severity", &self.log_min_severity)
            .field("log_event_name_allowlist", &self.log_event_name_allowlist)
            .field("log_event_ttl_secs", &self.log_event_ttl_secs)
            .field("dependency_ttl_secs", &self.dependency_ttl_secs)
            .field("max_services", &self.max_services)
            .field("max_metrics", &self.max_metrics)
            .field("max_dependencies", &self.max_dependencies)
            .field("max_log_events", &self.max_log_events)
            .field("reject_derived", &self.reject_derived)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("durability", &self.durability)
            .finish()
    }
}

impl Default for OtelSourceConfig {
    fn default() -> Self {
        Self {
            grpc_bind: default_grpc_bind(),
            http_bind: None,
            tls_cert_path: None,
            tls_key_path: None,
            tls_client_ca_path: None,
            auth_token: None,
            metric_allowlist: Vec::new(),
            metric_identity_attributes: Vec::new(),
            destination_attributes: default_destination_attributes(),
            span_kinds: default_span_kinds(),
            heartbeat_metric: None,
            heartbeat_event_name: None,
            log_min_severity: default_log_min_severity(),
            log_event_name_allowlist: Vec::new(),
            log_event_ttl_secs: default_log_event_ttl_secs(),
            dependency_ttl_secs: default_dependency_ttl_secs(),
            max_services: default_max_services(),
            max_metrics: default_max_metrics(),
            max_dependencies: default_max_dependencies(),
            max_log_events: default_max_log_events(),
            reject_derived: default_reject_derived(),
            max_request_bytes: default_max_request_bytes(),
            durability: None,
        }
    }
}

impl OtelSourceConfig {
    /// Validate bind addresses, TTLs, and TLS pairing.
    ///
    /// # Errors
    ///
    /// Returns an error if neither bind is set, a bind is not `host:port`, a TTL
    /// is zero, TLS paths are unpaired, or TLS files are missing.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.grpc_bind.trim().is_empty()
            && self.http_bind.as_ref().is_none_or(|s| s.trim().is_empty())
        {
            return Err(anyhow::anyhow!(
                "Validation error: at least one of grpc_bind or http_bind must be set"
            ));
        }
        if !self.grpc_bind.trim().is_empty() {
            parse_bind(&self.grpc_bind).context("invalid grpc_bind")?;
        }
        if let Some(http) = &self.http_bind {
            if !http.trim().is_empty() {
                parse_bind(http).context("invalid http_bind")?;
            }
        }
        if self.log_event_ttl_secs == 0 {
            return Err(anyhow::anyhow!("log_event_ttl_secs cannot be 0"));
        }
        if self.dependency_ttl_secs == 0 {
            return Err(anyhow::anyhow!("dependency_ttl_secs cannot be 0"));
        }
        if self.max_request_bytes == 0 {
            return Err(anyhow::anyhow!("max_request_bytes cannot be 0"));
        }
        match (&self.tls_cert_path, &self.tls_key_path) {
            (None, None) => {}
            (Some(cert), Some(key)) => {
                if !std::path::Path::new(cert).is_file() {
                    return Err(anyhow::anyhow!("tls_cert_path '{cert}' does not exist"));
                }
                if !std::path::Path::new(key).is_file() {
                    return Err(anyhow::anyhow!("tls_key_path '{key}' does not exist"));
                }
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "tls_cert_path and tls_key_path must be set together"
                ));
            }
        }
        if let Some(ca) = &self.tls_client_ca_path {
            if !std::path::Path::new(ca).is_file() {
                return Err(anyhow::anyhow!("tls_client_ca_path '{ca}' does not exist"));
            }
        }
        Ok(())
    }

    /// True when optional WAL durability is enabled.
    pub fn durability_enabled(&self) -> bool {
        self.durability.as_ref().is_some_and(|d| d.enabled)
    }
}

/// Parse `host:port` into a socket address string that `std::net` can parse.
pub fn parse_bind(bind: &str) -> anyhow::Result<std::net::SocketAddr> {
    bind.parse()
        .with_context(|| format!("bind address '{bind}' is not a valid host:port"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_binds_are_rejected() {
        let config = OtelSourceConfig {
            grpc_bind: String::new(),
            http_bind: None,
            ..OtelSourceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn zero_ttl_is_rejected() {
        let config = OtelSourceConfig {
            log_event_ttl_secs: 0,
            ..OtelSourceConfig::default()
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("log_event_ttl"));
        let config = OtelSourceConfig {
            dependency_ttl_secs: 0,
            ..OtelSourceConfig::default()
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("dependency_ttl"));
    }

    #[test]
    fn zero_max_request_bytes_is_rejected() {
        let config = OtelSourceConfig {
            max_request_bytes: 0,
            ..OtelSourceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn tls_paths_must_be_paired() {
        let config = OtelSourceConfig {
            tls_cert_path: Some("/tmp/missing-cert.pem".to_string()),
            tls_key_path: None,
            ..OtelSourceConfig::default()
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("together"));
    }

    #[test]
    fn invalid_bind_is_rejected() {
        let config = OtelSourceConfig {
            grpc_bind: "not-a-bind".to_string(),
            ..OtelSourceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_config_is_valid() {
        assert!(OtelSourceConfig::default().validate().is_ok());
    }
}
