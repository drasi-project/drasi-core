// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Protocol-independent settings and validation shared by network components.

use anyhow::{ensure, Result};
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use drasi_computation_contracts::ComponentId;
#[cfg(feature = "ingress")]
use drasi_computation_contracts::StreamId;
#[cfg(any(feature = "ingress", feature = "http-sink", feature = "grpc-sink"))]
use serde::{Deserialize, Serialize};
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use serde_json::Value;
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use std::collections::BTreeMap;
use std::time::Duration;

pub const MAX_INGRESS_CAPACITY: usize = 1_048_576;
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_RETRIES: u32 = 100;
/// Upper bound on simultaneously open listener connections.
pub const MAX_CONNECTIONS: usize = 65_536;
/// Upper bound on a per-client request rate or burst.
pub const MAX_RATE_LIMIT: u32 = 1_000_000;
/// Minimum length of a configured bearer token.
pub const MIN_AUTH_TOKEN_BYTES: usize = 16;
/// Upper bound on any configured PEM document.
pub const MAX_PEM_BYTES: usize = 256 * 1024;

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

/// Listeners bind the loopback interface unless configured otherwise.
#[cfg(feature = "ingress")]
pub fn host() -> String {
    "127.0.0.1".into()
}

#[cfg(feature = "ingress")]
pub fn ingress_capacity() -> usize {
    1024
}

#[cfg(feature = "ingress")]
pub fn max_connections() -> usize {
    1024
}

/// A configuration string, such as a token or private key, that is never
/// printed by `Debug`. Serialization is transparent: `configuration()` is the
/// persistence hook and must return the configured value.
#[cfg(feature = "ingress")]
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

#[cfg(feature = "ingress")]
impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[cfg(feature = "ingress")]
impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Per-client admission rate, in requests (and streamed messages) per second.
#[cfg(feature = "ingress")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub per_second: u32,
    pub burst: u32,
}

/// Validated PEM material for a TLS listener. A client CA enables mutual TLS:
/// clients must then present a certificate issued by that CA.
#[cfg(feature = "ingress")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsMaterial {
    pub certificate_pem: String,
    pub private_key_pem: SecretString,
    pub client_ca_pem: Option<String>,
}

/// Validated access-control settings shared by the HTTP and gRPC listeners.
#[cfg(feature = "ingress")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IngressSecurity {
    /// Accepted bearer tokens. Two tokens allow rotation without downtime.
    pub auth_tokens: Vec<SecretString>,
    /// Explicit opt-in to an unauthenticated listener on a non-loopback address.
    pub allow_unauthenticated: bool,
    pub rate_limit: Option<RateLimit>,
    pub tls: Option<TlsMaterial>,
}

/// Raw shared security fields, as they appear in a component configuration.
#[cfg(feature = "ingress")]
#[derive(Clone, Copy, Default)]
pub struct SecuritySettings<'a> {
    pub auth_token: Option<&'a SecretString>,
    pub auth_token_secondary: Option<&'a SecretString>,
    pub allow_unauthenticated: bool,
    pub rate_limit_per_second: Option<u32>,
    pub rate_limit_burst: Option<u32>,
    pub tls_certificate_pem: Option<&'a str>,
    pub tls_private_key_pem: Option<&'a SecretString>,
    pub tls_client_ca_pem: Option<&'a str>,
}

#[cfg(feature = "ingress")]
impl SecuritySettings<'_> {
    pub fn build(self) -> Result<IngressSecurity> {
        let mut auth_tokens = Vec::new();
        for token in [self.auth_token, self.auth_token_secondary]
            .into_iter()
            .flatten()
        {
            let value = token.expose();
            ensure!(
                value.len() >= MIN_AUTH_TOKEN_BYTES
                    && value.len() <= 4096
                    && value.bytes().all(|byte| byte.is_ascii_graphic()),
                "auth tokens must be 16..=4096 visible ASCII characters"
            );
            auth_tokens.push(token.clone());
        }
        ensure!(
            self.auth_token.is_some() || self.auth_token_secondary.is_none(),
            "authTokenSecondary requires authToken"
        );
        ensure!(
            auth_tokens.len() < 2 || auth_tokens[0] != auth_tokens[1],
            "authToken and authTokenSecondary must differ"
        );
        let rate_limit = match (self.rate_limit_per_second, self.rate_limit_burst) {
            (None, None) => None,
            (None, Some(_)) => anyhow::bail!("rateLimitBurst requires rateLimitPerSecond"),
            (Some(per_second), burst) => {
                let burst = burst.unwrap_or(per_second);
                ensure!(
                    (1..=MAX_RATE_LIMIT).contains(&per_second)
                        && (1..=MAX_RATE_LIMIT).contains(&burst),
                    "rateLimitPerSecond and rateLimitBurst must be in 1..=1000000"
                );
                Some(RateLimit { per_second, burst })
            }
        };
        let tls = match (self.tls_certificate_pem, self.tls_private_key_pem) {
            (None, None) => {
                ensure!(
                    self.tls_client_ca_pem.is_none(),
                    "tlsClientCaPem requires tlsCertificatePem and tlsPrivateKeyPem"
                );
                None
            }
            (Some(certificate), Some(key)) => {
                let material = TlsMaterial {
                    certificate_pem: certificate.to_owned(),
                    private_key_pem: key.clone(),
                    client_ca_pem: self.tls_client_ca_pem.map(str::to_owned),
                };
                material.validate()?;
                Some(material)
            }
            _ => {
                anyhow::bail!("tlsCertificatePem and tlsPrivateKeyPem must be configured together")
            }
        };
        Ok(IngressSecurity {
            auth_tokens,
            allow_unauthenticated: self.allow_unauthenticated,
            rate_limit,
            tls,
        })
    }
}

/// Validated connection and admission settings, without protocol-specific options.
#[cfg(feature = "ingress")]
#[derive(Clone, Debug)]
pub struct IngressConfig {
    pub stream: StreamId,
    pub host: String,
    pub port: u16,
    pub timeout_ms: u64,
    pub ingress_capacity: usize,
    pub max_message_bytes: usize,
    pub max_connections: usize,
    pub security: IngressSecurity,
}

#[cfg(feature = "ingress")]
impl IngressConfig {
    pub fn validate(&self) -> Result<()> {
        self.ip()?;
        duration(self.timeout_ms, "timeoutMs")?;
        ensure!(
            (1..=MAX_INGRESS_CAPACITY).contains(&self.ingress_capacity),
            "ingressCapacity must be in 1..=1048576"
        );
        ensure!(
            (1..=MAX_MESSAGE_BYTES).contains(&self.max_message_bytes),
            "maxMessageBytes must be in 1..=33554432"
        );
        ensure!(
            (1..=MAX_CONNECTIONS).contains(&self.max_connections),
            "maxConnections must be in 1..=65536"
        );
        if let Some(tls) = &self.security.tls {
            tls.validate()?;
        }
        Ok(())
    }

    pub fn ip(&self) -> Result<std::net::IpAddr> {
        self.host
            .parse()
            .map_err(|_| anyhow::anyhow!("host must be a literal IP address"))
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// Refuses an unauthenticated listener on a non-loopback address unless
    /// `allowUnauthenticated` is explicit. `token_protected` states whether the
    /// routes that admit data in this mode rely on the shared bearer token.
    pub fn validate_exposure(&self, token_protected: bool) -> Result<()> {
        if self.ip()?.is_loopback() || self.security.allow_unauthenticated {
            return Ok(());
        }
        ensure!(
            !token_protected || !self.security.auth_tokens.is_empty(),
            "listening on non-loopback address {} requires authToken, or allowUnauthenticated: true behind an authenticating proxy",
            self.host
        );
        Ok(())
    }
}

/// Field descriptions shared by both source listeners. The caller supplies
/// its protocol's default port and message-size limit.
#[cfg(feature = "ingress")]
pub fn ingress_options(
    default_port: u16,
    default_message_bytes: usize,
    message_bytes_description: &str,
) -> Vec<drasi_computation_plugin_sdk::ConfigOption> {
    use drasi_computation_plugin_sdk::{ConfigOption as Field, ConfigType::*};
    vec![
        Field::new("stream", String, true, "Nonempty output stream identifier.").default_description("Required; no default."),
        Field::new("host", String, false, "Literal IP address to listen on. A non-loopback address requires authToken, or allowUnauthenticated: true.").default(host()),
        Field::new("port", Integer, false, "TCP port; zero requests an available local port.").default(default_port).minimum(0).maximum(u16::MAX as u64),
        Field::new("timeoutMs", Integer, false, "Request-body and admission deadline in milliseconds.").default(timeout_ms()).minimum(1),
        Field::new("ingressCapacity", Integer, false, "Maximum simultaneous requests being admitted; also bounds the volatile ingress queue.").default(ingress_capacity()).minimum(1).maximum(MAX_INGRESS_CAPACITY as u64),
        Field::new("maxMessageBytes", Integer, false, message_bytes_description).default(default_message_bytes).minimum(1).maximum(MAX_MESSAGE_BYTES as u64),
        Field::new("maxConnections", Integer, false, "Maximum simultaneously open client connections; further connections are closed immediately.").default(max_connections()).minimum(1).maximum(MAX_CONNECTIONS as u64),
        Field::new("authToken", String, false, "Bearer token required on data and admission routes (Authorization: Bearer <token>). At least 16 visible ASCII characters.").secret(),
        Field::new("authTokenSecondary", String, false, "Second accepted bearer token, for rotation without downtime. Requires authToken.").secret(),
        Field::new("allowUnauthenticated", Boolean, false, "Permit a non-loopback listener without authToken, for deployments behind an authenticating proxy.").default(false),
        Field::new("rateLimitPerSecond", Integer, false, "Per-client-address request rate limit; streamed messages count individually. Unset disables rate limiting.").minimum(1).maximum(MAX_RATE_LIMIT as u64),
        Field::new("rateLimitBurst", Integer, false, "Per-client-address burst allowance. Defaults to rateLimitPerSecond.").minimum(1).maximum(MAX_RATE_LIMIT as u64),
        Field::new("tlsCertificatePem", String, false, "PEM certificate chain served by the TLS listener. Requires tlsPrivateKeyPem."),
        Field::new("tlsPrivateKeyPem", String, false, "PEM private key for tlsCertificatePem.").secret(),
        Field::new("tlsClientCaPem", String, false, "PEM CA bundle; when set, clients must present a certificate issued by it (mutual TLS)."),
    ]
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
        component: &ComponentId,
        output: &str,
        sequence: u64,
    ) -> Result<()> {
        match (self, result) {
            (_, Ok(())) => Ok(()),
            (Self::Strict, Err(error)) => Err(error),
            (Self::Skip, Err(error)) => {
                crate::diag!(Warn, "deliberate delivery skip: component={component} output={output} sequence={sequence} error={error:#}");
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

#[cfg(all(test, feature = "ingress"))]
mod tests {
    use super::*;

    fn token(value: &str) -> SecretString {
        SecretString::new(value)
    }

    #[test]
    fn secrets_never_print() {
        assert_eq!(format!("{:?}", token("a-very-secret-token")), "<redacted>");
    }

    #[test]
    fn security_settings_validate_tokens_rate_limits_and_tls_pairs() {
        let primary = token("0123456789abcdef");
        let short = token("short");
        assert!(SecuritySettings {
            auth_token: Some(&short),
            ..Default::default()
        }
        .build()
        .is_err());
        assert!(SecuritySettings {
            auth_token_secondary: Some(&primary),
            ..Default::default()
        }
        .build()
        .is_err());
        assert!(SecuritySettings {
            auth_token: Some(&primary),
            auth_token_secondary: Some(&primary),
            ..Default::default()
        }
        .build()
        .is_err());
        assert!(SecuritySettings {
            rate_limit_burst: Some(10),
            ..Default::default()
        }
        .build()
        .is_err());
        assert_eq!(
            SecuritySettings {
                rate_limit_per_second: Some(5),
                ..Default::default()
            }
            .build()
            .expect("rate limit")
            .rate_limit,
            Some(RateLimit {
                per_second: 5,
                burst: 5
            })
        );
        assert!(SecuritySettings {
            tls_certificate_pem: Some("cert"),
            ..Default::default()
        }
        .build()
        .is_err());
        assert!(SecuritySettings {
            tls_client_ca_pem: Some("ca"),
            ..Default::default()
        }
        .build()
        .is_err());
    }

    fn config(host: &str, security: IngressSecurity) -> IngressConfig {
        IngressConfig {
            stream: StreamId::try_new("events").expect("stream"),
            host: host.into(),
            port: 0,
            timeout_ms: 1000,
            ingress_capacity: 8,
            max_message_bytes: 1024,
            max_connections: 8,
            security,
        }
    }

    #[test]
    fn non_loopback_exposure_requires_a_token_or_explicit_opt_in() {
        assert!(config("127.0.0.1", IngressSecurity::default())
            .validate_exposure(true)
            .is_ok());
        assert!(config("::1", IngressSecurity::default())
            .validate_exposure(true)
            .is_ok());
        assert!(config("0.0.0.0", IngressSecurity::default())
            .validate_exposure(true)
            .is_err());
        assert!(config("0.0.0.0", IngressSecurity::default())
            .validate_exposure(false)
            .is_ok());
        assert!(config(
            "0.0.0.0",
            IngressSecurity {
                auth_tokens: vec![token("0123456789abcdef")],
                ..Default::default()
            }
        )
        .validate_exposure(true)
        .is_ok());
        assert!(config(
            "0.0.0.0",
            IngressSecurity {
                allow_unauthenticated: true,
                ..Default::default()
            }
        )
        .validate_exposure(true)
        .is_ok());
        assert!(config("localhost", IngressSecurity::default())
            .validate()
            .is_err());
    }
}
