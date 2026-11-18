// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration for `drasi.network/otel-source`.
//!
//! The configuration has two parts: listener settings (address, protocols,
//! limits, access control and TLS), shared with the other native network
//! sources, and projection settings that decide which OTLP records become
//! graph elements. [`OtelSourceConfig::projection`] extracts the latter as
//! the [`Projection`] read per request.

use anyhow::{ensure, Result};
use drasi_computation_contracts::StreamId;
use drasi_computation_network_common::config::{
    self as shared, IngressConfig, SecretString, SecuritySettings,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Standard OTLP/gRPC port.
pub const DEFAULT_GRPC_PORT: u16 = 4317;
/// Standard OTLP/HTTP port.
pub const DEFAULT_HTTP_PORT: u16 = 4318;
/// Lower bound on `ingressCapacity`: one projected group (a record's
/// Service, Heartbeat, Metric and relations) is admitted all-or-nothing and
/// must fit in the queue.
pub const MIN_INGRESS_CAPACITY: usize = 8;
/// Upper bound on any time-to-live: one year.
pub const MAX_TTL_SECS: u64 = 31_536_000;
/// Upper bound on any cardinality cap.
pub const MAX_CARDINALITY: usize = 10_000_000;
/// Upper bound on entries in a pattern or attribute list.
pub const MAX_LIST_ENTRIES: usize = 1024;
/// Upper bound on one pattern, attribute key or name.
pub const MAX_NAME_BYTES: usize = 256;

/// An OTLP transport served by the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// OTLP/gRPC (`opentelemetry.proto.collector.*.v1` services).
    Grpc,
    /// OTLP/HTTP with binary protobuf bodies (`POST /v1/{metrics,traces,logs}`).
    Http,
}

/// An OTLP span kind that may produce a `DEPENDS_ON` relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SpanKind {
    Unspecified,
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

impl SpanKind {
    /// The OTLP `Span.SpanKind` number.
    pub fn number(self) -> i32 {
        use crate::otlp::proto::trace::v1::span::SpanKind as Otlp;
        (match self {
            Self::Unspecified => Otlp::Unspecified,
            Self::Internal => Otlp::Internal,
            Self::Server => Otlp::Server,
            Self::Client => Otlp::Client,
            Self::Producer => Otlp::Producer,
            Self::Consumer => Otlp::Consumer,
        }) as i32
    }
}

/// The minimum severity of an admitted log record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Severity {
    /// The lowest OTLP `SeverityNumber` of this level (`TRACE` = 1 ... `FATAL` = 21).
    pub fn number(self) -> i32 {
        use crate::otlp::proto::logs::v1::SeverityNumber as Otlp;
        (match self {
            Self::Trace => Otlp::Trace,
            Self::Debug => Otlp::Debug,
            Self::Info => Otlp::Info,
            Self::Warn => Otlp::Warn,
            Self::Error => Otlp::Error,
            Self::Fatal => Otlp::Fatal,
        }) as i32
    }
}

fn protocols() -> Vec<Protocol> {
    vec![Protocol::Grpc]
}
fn grpc_port() -> u16 {
    DEFAULT_GRPC_PORT
}
fn http_port() -> u16 {
    DEFAULT_HTTP_PORT
}
fn message_bytes() -> usize {
    4 * 1024 * 1024
}
fn header_read_timeout_ms() -> u64 {
    10_000
}
fn destination_attributes() -> Vec<String> {
    vec!["peer.service".into()]
}
fn span_kinds() -> Vec<SpanKind> {
    vec![SpanKind::Client]
}
fn log_min_severity() -> Severity {
    Severity::Error
}
fn log_event_ttl_secs() -> u64 {
    60
}
fn dependency_ttl_secs() -> u64 {
    300
}
fn max_services() -> usize {
    1000
}
fn max_metrics() -> usize {
    2000
}
fn max_dependencies() -> usize {
    5000
}
fn max_log_events() -> usize {
    5000
}
fn reject_derived() -> bool {
    true
}

/// Settings for one native OpenTelemetry source.
///
/// Secret fields (`authToken`, `authTokenSecondary`, `tlsPrivateKeyPem`) are
/// supplied as host secret references and are never printed by `Debug`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OtelSourceConfig {
    /// Output stream identifier.
    pub stream: StreamId,
    /// Literal IP address both listeners bind.
    #[serde(default = "shared::host")]
    pub host: String,
    /// OTLP transports to serve; at least one, without duplicates.
    #[serde(default = "protocols")]
    pub protocols: Vec<Protocol>,
    /// OTLP/gRPC port, used when `protocols` contains `grpc`; zero picks one.
    #[serde(default = "grpc_port")]
    pub grpc_port: u16,
    /// OTLP/HTTP port, used when `protocols` contains `http`; zero picks one.
    #[serde(default = "http_port")]
    pub http_port: u16,
    /// Request-body and admission deadline in milliseconds.
    #[serde(default = "shared::timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum simultaneous export requests; also the volatile queue bound.
    #[serde(default = "shared::ingress_capacity")]
    pub ingress_capacity: usize,
    /// Maximum export request size in bytes, before and after decompression.
    #[serde(default = "message_bytes")]
    pub max_message_bytes: usize,
    /// Maximum simultaneously open client connections, per listener.
    #[serde(default = "shared::max_connections")]
    pub max_connections: usize,
    /// Deadline for a client to finish TLS and send its first request headers
    /// (HTTP) or the HTTP/2 connection preface (gRPC).
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

    /// Metric names that become `Metric` nodes; `*` is the only wildcard.
    /// Empty admits no metric.
    #[serde(default)]
    pub metric_allowlist: Vec<String>,
    /// Data-point attribute keys that become part of a Metric's identity.
    #[serde(default)]
    pub metric_identity_attributes: Vec<String>,
    /// Span attribute keys tried in order to name a span's callee.
    #[serde(default = "destination_attributes")]
    pub destination_attributes: Vec<String>,
    /// Span kinds that may produce `DEPENDS_ON`.
    #[serde(default = "span_kinds")]
    pub span_kinds: Vec<SpanKind>,
    /// Metric name that refreshes a service's `Heartbeat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_metric: Option<String>,
    /// Log `event_name` that refreshes a service's `Heartbeat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_event_name: Option<String>,
    /// Minimum severity of a log record that becomes a `LogEvent`.
    #[serde(default = "log_min_severity")]
    pub log_min_severity: Severity,
    /// When nonempty, only matching log `event_name`s become `LogEvent`s.
    #[serde(default)]
    pub log_event_name_allowlist: Vec<String>,
    /// `LogEvent` lifetime in seconds from receipt.
    #[serde(default = "log_event_ttl_secs")]
    pub log_event_ttl_secs: u64,
    /// `DEPENDS_ON` lifetime in seconds from the last refreshing span's receipt.
    #[serde(default = "dependency_ttl_secs")]
    pub dependency_ttl_secs: u64,
    /// Maximum distinct live `Service` nodes.
    #[serde(default = "max_services")]
    pub max_services: usize,
    /// Maximum distinct live `Metric` nodes.
    #[serde(default = "max_metrics")]
    pub max_metrics: usize,
    /// Maximum live `DEPENDS_ON` relations.
    #[serde(default = "max_dependencies")]
    pub max_dependencies: usize,
    /// Maximum live `LogEvent` nodes.
    #[serde(default = "max_log_events")]
    pub max_log_events: usize,
    /// Drop records whose resource or attributes carry
    /// `drasi.source.origin=derived`.
    #[serde(default = "reject_derived")]
    pub reject_derived: bool,
}

/// The projection settings, read per request and replaceable at a Pause.
#[derive(Clone, Debug, PartialEq)]
pub struct Projection {
    pub metric_allowlist: Vec<String>,
    pub metric_identity_attributes: Vec<String>,
    pub destination_attributes: Vec<String>,
    pub span_kinds: Vec<i32>,
    pub heartbeat_metric: Option<String>,
    pub heartbeat_event_name: Option<String>,
    pub log_min_severity: i32,
    pub log_event_name_allowlist: Vec<String>,
    pub log_event_ttl_secs: u64,
    pub dependency_ttl_secs: u64,
    pub max_services: usize,
    pub max_metrics: usize,
    pub max_dependencies: usize,
    pub max_log_events: usize,
    pub reject_derived: bool,
}

impl OtelSourceConfig {
    /// The published configuration description.
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        // The shared listener fields, with the single `port` replaced by one
        // port per protocol.
        let mut fields: Vec<Field> = shared::ingress_options(
            0,
            message_bytes(),
            "Maximum export request size in bytes, before and after gzip decompression.",
        )
        .into_iter()
        .filter(|field| !matches!(field.name(), "port" | "ingressCapacity"))
        .collect();
        let list = |name: &str, description: &str| {
            Field::new(name, Array, false, description).default(serde_json::json!([]))
        };
        fields.extend([
            Field::new("ingressCapacity", Integer, false, "Maximum simultaneous export requests being admitted; also bounds the volatile ingress queue.").default(shared::ingress_capacity()).minimum(MIN_INGRESS_CAPACITY as i64).maximum(shared::MAX_INGRESS_CAPACITY as u64),
            Field::new("protocols", Array, false, "OTLP transports to serve: \"grpc\" and/or \"http\" (binary protobuf). At least one, without duplicates.").default(serde_json::json!(["grpc"])),
            Field::new("grpcPort", Integer, false, "OTLP/gRPC port, used when protocols contains grpc. Zero requests an available port.").default(grpc_port()).minimum(0).maximum(u16::MAX as u64),
            Field::new("httpPort", Integer, false, "OTLP/HTTP port, used when protocols contains http. Zero requests an available port.").default(http_port()).minimum(0).maximum(u16::MAX as u64),
            Field::new("headerReadTimeoutMs", Integer, false, "Deadline in milliseconds for a client to finish the TLS handshake and send its request headers or HTTP/2 preface.").default(header_read_timeout_ms()).minimum(1).maximum(600_000),
            list("metricAllowlist", "Metric names that become Metric nodes. '*' is the only wildcard; an empty list admits no metric."),
            list("metricIdentityAttributes", "Data-point attribute keys appended to a Metric's identity, so each combination of their values is its own node."),
            Field::new("destinationAttributes", Array, false, "Span attribute keys tried in order to name a span's callee; the first nonempty value wins. Empty disables DEPENDS_ON.").default(destination_attributes()),
            Field::new("spanKinds", Array, false, "Span kinds that may produce DEPENDS_ON: UNSPECIFIED, INTERNAL, SERVER, CLIENT, PRODUCER, CONSUMER.").default(serde_json::json!(["CLIENT"])),
            Field::new("heartbeatMetric", String, false, "Metric name that refreshes a service's Heartbeat node; it need not be allowlisted."),
            Field::new("heartbeatEventName", String, false, "Log event_name that refreshes a service's Heartbeat node, regardless of severity."),
            Field::new("logMinSeverity", String, false, "Minimum severity of a LogEvent: TRACE, DEBUG, INFO, WARN, ERROR or FATAL.").default("ERROR"),
            list("logEventNameAllowlist", "When nonempty, only log records whose event_name matches become LogEvents. '*' is the only wildcard."),
            Field::new("logEventTtlSecs", Integer, false, "LogEvent lifetime in seconds from receipt.").default(log_event_ttl_secs()).minimum(1).maximum(MAX_TTL_SECS),
            Field::new("dependencyTtlSecs", Integer, false, "DEPENDS_ON lifetime in seconds from the last refreshing span's receipt.").default(dependency_ttl_secs()).minimum(1).maximum(MAX_TTL_SECS),
            Field::new("maxServices", Integer, false, "Maximum distinct live Service nodes.").default(max_services()).minimum(1).maximum(MAX_CARDINALITY as u64),
            Field::new("maxMetrics", Integer, false, "Maximum distinct live Metric nodes.").default(max_metrics()).minimum(1).maximum(MAX_CARDINALITY as u64),
            Field::new("maxDependencies", Integer, false, "Maximum live DEPENDS_ON relations.").default(max_dependencies()).minimum(1).maximum(MAX_CARDINALITY as u64),
            Field::new("maxLogEvents", Integer, false, "Maximum live LogEvent nodes.").default(max_log_events()).minimum(1).maximum(MAX_CARDINALITY as u64),
            Field::new("rejectDerived", Boolean, false, "Drop records whose resource or attributes carry drasi.source.origin=derived, so exported Drasi output cannot loop back.").default(reject_derived()),
        ]);
        ConfigDefinition::try_new(fields)
            .unwrap_or_else(|error| unreachable!("constant OTel source configuration: {error}"))
    }

    /// Deserializes and validates a configuration.
    pub fn from_value(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    /// Checks every domain constraint the schema cannot express.
    pub fn validate(&self) -> Result<()> {
        self.ingress()?.validate()?;
        ensure!(
            self.ingress_capacity >= MIN_INGRESS_CAPACITY,
            "ingressCapacity must be at least {MIN_INGRESS_CAPACITY}"
        );
        ensure!(
            !self.protocols.is_empty(),
            "protocols must name at least one transport"
        );
        ensure!(
            self.protocols.len() == 1 || self.protocols[0] != self.protocols[1],
            "protocols must not repeat a transport"
        );
        ensure!(
            self.protocols.len() <= 2,
            "protocols must not repeat a transport"
        );
        if self.serves(Protocol::Grpc) && self.serves(Protocol::Http) {
            ensure!(
                self.grpc_port == 0 || self.grpc_port != self.http_port,
                "grpcPort and httpPort must differ"
            );
        }
        ensure!(
            (1..=600_000).contains(&self.header_read_timeout_ms),
            "headerReadTimeoutMs must be in 1..=600000"
        );
        names("metricAllowlist", &self.metric_allowlist)?;
        names("metricIdentityAttributes", &self.metric_identity_attributes)?;
        names("destinationAttributes", &self.destination_attributes)?;
        names("logEventNameAllowlist", &self.log_event_name_allowlist)?;
        ensure!(
            self.span_kinds.len() <= 6,
            "spanKinds must not repeat a kind"
        );
        for (index, kind) in self.span_kinds.iter().enumerate() {
            ensure!(
                !self.span_kinds[..index].contains(kind),
                "spanKinds must not repeat a kind"
            );
        }
        for (field, value) in [
            ("heartbeatMetric", &self.heartbeat_metric),
            ("heartbeatEventName", &self.heartbeat_event_name),
        ] {
            if let Some(value) = value {
                name(field, value)?;
            }
        }
        for (field, value) in [
            ("logEventTtlSecs", self.log_event_ttl_secs),
            ("dependencyTtlSecs", self.dependency_ttl_secs),
        ] {
            ensure!(
                (1..=MAX_TTL_SECS).contains(&value),
                "{field} must be in 1..={MAX_TTL_SECS}"
            );
        }
        for (field, value) in [
            ("maxServices", self.max_services),
            ("maxMetrics", self.max_metrics),
            ("maxDependencies", self.max_dependencies),
            ("maxLogEvents", self.max_log_events),
        ] {
            ensure!(
                (1..=MAX_CARDINALITY).contains(&value),
                "{field} must be in 1..={MAX_CARDINALITY}"
            );
        }
        Ok(())
    }

    /// Whether `protocol` is served.
    pub fn serves(&self, protocol: Protocol) -> bool {
        self.protocols.contains(&protocol)
    }

    /// The listener ports, in `protocols` order.
    pub fn ports(&self) -> Vec<u16> {
        self.protocols
            .iter()
            .map(|protocol| match protocol {
                Protocol::Grpc => self.grpc_port,
                Protocol::Http => self.http_port,
            })
            .collect()
    }

    /// The shared listener settings. `port` is the first protocol's port;
    /// the source binds every protocol's port itself.
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
            port: self.ports().first().copied().unwrap_or(self.grpc_port),
            timeout_ms: self.timeout_ms,
            ingress_capacity: self.ingress_capacity,
            max_message_bytes: self.max_message_bytes,
            max_connections: self.max_connections,
            security,
        })
    }

    /// Refuses an unauthenticated listener on a non-loopback address. Every
    /// OTLP export admits data, so every route is token-protected.
    pub fn validate_exposure(&self) -> Result<()> {
        self.ingress()?.validate_exposure(true)
    }

    /// The projection settings.
    pub fn projection(&self) -> Projection {
        Projection {
            metric_allowlist: self.metric_allowlist.clone(),
            metric_identity_attributes: self.metric_identity_attributes.clone(),
            destination_attributes: self.destination_attributes.clone(),
            span_kinds: self.span_kinds.iter().map(|kind| kind.number()).collect(),
            heartbeat_metric: self.heartbeat_metric.clone(),
            heartbeat_event_name: self.heartbeat_event_name.clone(),
            log_min_severity: self.log_min_severity.number(),
            log_event_name_allowlist: self.log_event_name_allowlist.clone(),
            log_event_ttl_secs: self.log_event_ttl_secs,
            dependency_ttl_secs: self.dependency_ttl_secs,
            max_services: self.max_services,
            max_metrics: self.max_metrics,
            max_dependencies: self.max_dependencies,
            max_log_events: self.max_log_events,
            reject_derived: self.reject_derived,
        }
    }
}

fn name(field: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_NAME_BYTES,
        "{field} entries must be 1..={MAX_NAME_BYTES} bytes"
    );
    Ok(())
}

fn names(field: &str, values: &[String]) -> Result<()> {
    ensure!(
        values.len() <= MAX_LIST_ENTRIES,
        "{field} must have at most {MAX_LIST_ENTRIES} entries"
    );
    for value in values {
        name(field, value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with(extra: Value) -> Value {
        let mut value = json!({"stream": "otel"});
        value
            .as_object_mut()
            .expect("object")
            .extend(extra.as_object().expect("object").clone());
        value
    }

    #[test]
    fn defaults_serialize_explicitly_and_bind_loopback() -> Result<()> {
        let config = OtelSourceConfig::from_value(json!({"stream": "otel"}))?;
        assert_eq!(
            serde_json::to_value(&config)?,
            json!({
                "stream": "otel", "host": "127.0.0.1", "protocols": ["grpc"],
                "grpcPort": 4317, "httpPort": 4318, "timeoutMs": 60000,
                "ingressCapacity": 1024, "maxMessageBytes": 4194304,
                "maxConnections": 1024, "headerReadTimeoutMs": 10000,
                "allowUnauthenticated": false, "metricAllowlist": [],
                "metricIdentityAttributes": [], "destinationAttributes": ["peer.service"],
                "spanKinds": ["CLIENT"], "logMinSeverity": "ERROR",
                "logEventNameAllowlist": [], "logEventTtlSecs": 60,
                "dependencyTtlSecs": 300, "maxServices": 1000, "maxMetrics": 2000,
                "maxDependencies": 5000, "maxLogEvents": 5000, "rejectDerived": true
            })
        );
        assert_eq!(config.ports(), vec![4317]);
        Ok(())
    }

    #[test]
    fn rejects_invalid_unknown_and_removed_settings() {
        for extra in [
            json!({"unknown": true}),
            json!({"grpcBind": "0.0.0.0:4317"}),
            json!({"httpBind": "0.0.0.0:4318"}),
            json!({"tlsCertPath": "/certs/server.crt"}),
            json!({"maxRequestBytes": 1}),
            json!({"durability": {"enabled": true}}),
            json!({"host": "localhost"}),
            json!({"protocols": []}),
            json!({"protocols": ["grpc", "grpc"]}),
            json!({"protocols": ["json"]}),
            json!({"protocols": ["grpc", "http"], "grpcPort": 5000, "httpPort": 5000}),
            json!({"timeoutMs": 0}),
            json!({"ingressCapacity": 0}),
            json!({"ingressCapacity": 7}),
            json!({"maxMessageBytes": 33554433}),
            json!({"headerReadTimeoutMs": 0}),
            json!({"authToken": "short"}),
            json!({"metricAllowlist": [""]}),
            json!({"metricIdentityAttributes": ["x".repeat(257)]}),
            json!({"destinationAttributes": [""]}),
            json!({"spanKinds": ["CLIENT", "CLIENT"]}),
            json!({"spanKinds": ["client"]}),
            json!({"heartbeatMetric": ""}),
            json!({"logMinSeverity": "WARNING"}),
            json!({"logMinSeverity": "error"}),
            json!({"logEventTtlSecs": 0}),
            json!({"dependencyTtlSecs": 31536001}),
            json!({"maxServices": 0}),
            json!({"maxLogEvents": 10000001}),
        ] {
            let value = with(extra);
            assert!(
                OtelSourceConfig::from_value(value.clone()).is_err(),
                "{value}"
            );
        }
        let too_many: Vec<String> = (0..=MAX_LIST_ENTRIES).map(|n| format!("m{n}")).collect();
        assert!(OtelSourceConfig::from_value(with(json!({"metricAllowlist": too_many}))).is_err());
    }

    #[test]
    fn both_protocols_bind_their_own_ports_in_order() -> Result<()> {
        let config = OtelSourceConfig::from_value(with(json!({
            "protocols": ["http", "grpc"], "grpcPort": 9001, "httpPort": 9002
        })))?;
        assert_eq!(config.ports(), vec![9002, 9001]);
        assert_eq!(config.ingress()?.port, 9002);
        // Ephemeral ports may coincide in configuration.
        OtelSourceConfig::from_value(with(json!({
            "protocols": ["grpc", "http"], "grpcPort": 0, "httpPort": 0
        })))?;
        Ok(())
    }

    #[test]
    fn projection_maps_enums_to_otlp_numbers() -> Result<()> {
        let config = OtelSourceConfig::from_value(with(json!({
            "spanKinds": ["CLIENT", "PRODUCER", "UNSPECIFIED"],
            "logMinSeverity": "WARN"
        })))?;
        let projection = config.projection();
        assert_eq!(projection.span_kinds, vec![3, 4, 0]);
        assert_eq!(projection.log_min_severity, 13);
        assert_eq!(Severity::Trace.number(), 1);
        assert_eq!(Severity::Fatal.number(), 21);
        Ok(())
    }

    #[test]
    fn non_loopback_exposure_requires_authentication() -> Result<()> {
        assert!(
            OtelSourceConfig::from_value(with(json!({"host": "0.0.0.0"})))?
                .validate_exposure()
                .is_err()
        );
        OtelSourceConfig::from_value(with(
            json!({"host": "0.0.0.0", "authToken": "0123456789abcdef"}),
        ))?
        .validate_exposure()?;
        OtelSourceConfig::from_value(with(
            json!({"host": "0.0.0.0", "allowUnauthenticated": true}),
        ))?
        .validate_exposure()?;
        Ok(())
    }

    #[test]
    fn debug_never_prints_secrets() -> Result<()> {
        let config =
            OtelSourceConfig::from_value(with(json!({"authToken": "super-secret-token-value"})))?;
        assert!(!format!("{config:?}").contains("super-secret-token-value"));
        Ok(())
    }

    #[test]
    fn published_definition_is_valid() {
        let details = OtelSourceConfig::definition().details();
        assert!(!details.fields.contains_key("port"));
        assert!(details.fields.contains_key("grpcPort"));
        // Published enum defaults agree with the serde defaults.
        assert_eq!(
            details.fields["protocols"].default,
            Some(serde_json::json!(["grpc"]))
        );
        assert_eq!(
            details.fields["spanKinds"].default,
            Some(serde_json::json!(["CLIENT"]))
        );
        assert_eq!(
            details.fields["logMinSeverity"].default,
            Some(serde_json::json!("ERROR"))
        );
    }
}
