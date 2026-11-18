// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Protocol-independent settings and validation shared by network components.

use anyhow::{ensure, Result};
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
use drasi_computation_contracts::ComponentId;
#[cfg(feature = "ingress")]
use drasi_computation_contracts::StreamId;
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
#[cfg(feature = "listener")]
pub fn host() -> String {
    "127.0.0.1".into()
}

#[cfg(feature = "ingress")]
pub fn ingress_capacity() -> usize {
    1024
}

#[cfg(feature = "listener")]
pub fn max_connections() -> usize {
    1024
}

/// A configuration string, such as a token or private key, that is never
/// printed by `Debug`. Serialization is transparent: `configuration()` is the
/// persistence hook and must return the configured value.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Per-client admission rate, in requests (and streamed messages) per second.
#[cfg(feature = "listener")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub per_second: u32,
    pub burst: u32,
}

/// Validated PEM material for a TLS listener. A client CA enables mutual TLS:
/// clients must then present a certificate issued by that CA.
#[cfg(feature = "listener")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsMaterial {
    pub certificate_pem: String,
    pub private_key_pem: SecretString,
    pub client_ca_pem: Option<String>,
}

/// Validated access-control settings shared by the HTTP and gRPC listeners.
#[cfg(feature = "listener")]
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
#[cfg(feature = "listener")]
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

#[cfg(feature = "listener")]
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

/// Upper bound on one logged field, so logs never carry whole bodies.
#[cfg(feature = "sink")]
pub const MAX_LOG_FIELD_LEN: usize = 512;

/// A log-safe rendering of untrusted text: control characters escaped and
/// the length capped at [`MAX_LOG_FIELD_LEN`] bytes.
#[cfg(feature = "sink")]
pub fn sanitize(text: &str) -> String {
    let mut result = String::with_capacity(text.len().min(MAX_LOG_FIELD_LEN));
    for character in text.chars() {
        if character.is_control() {
            result.extend(character.escape_default());
        } else {
            result.push(character);
        }
        if result.len() >= MAX_LOG_FIELD_LEN {
            let mut end = MAX_LOG_FIELD_LEN;
            while !result.is_char_boundary(end) {
                end -= 1;
            }
            result.truncate(end);
            result.push_str("...");
            break;
        }
    }
    result
}

/// Header and gRPC metadata names carrying the real component and output port
/// that produced each request's data.
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub const COMPONENT_HEADER: &str = "x-drasi-component";
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub const PORT_HEADER: &str = "x-drasi-port";
/// Header and gRPC metadata name carrying a request's stable delivery
/// identity; see [`delivery_id`].
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub const DELIVERY_ID_HEADER: &str = "x-drasi-delivery-id";

/// Names a sink sets itself; configuration and templates may not set them.
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub const RESERVED_HEADERS: [&str; 3] = [COMPONENT_HEADER, PORT_HEADER, DELIVERY_ID_HEADER];

/// The stable identity of the changes `first..=last` from one output:
/// `{component}/{port}/{sequence}.{ordinal}` for one change, or
/// `{component}/{port}/{sequence}.{ordinal}-{sequence}.{ordinal}` for a
/// contiguous run. The producer's logical sequence and the change ordinal are
/// replayed unchanged, so a retried or replayed delivery carries the same
/// identity and a receiver can deduplicate it. It contains no transport,
/// retry or configuration identity.
#[cfg(any(feature = "http-sink", feature = "grpc-sink"))]
pub fn delivery_id(
    first: &drasi_computation_contracts::ChangeNotification,
    last: &drasi_computation_contracts::ChangeNotification,
) -> String {
    let output = first.output();
    let start = (first.sequence(), first.change().ordinal());
    let end = (last.sequence(), last.change().ordinal());
    if start == end {
        format!(
            "{}/{}/{}.{}",
            output.component, output.port, start.0, start.1
        )
    } else {
        format!(
            "{}/{}/{}.{}-{}.{}",
            output.component, output.port, start.0, start.1, end.0, end.1
        )
    }
}

/// Validated client TLS settings for an outbound connection: an optional
/// private CA to trust in addition to the system roots, and an optional
/// client certificate and key for mutual TLS.
#[cfg(feature = "sink")]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientTls {
    pub ca_pem: Option<String>,
    pub certificate_pem: Option<String>,
    pub private_key_pem: Option<SecretString>,
}

#[cfg(feature = "sink")]
impl ClientTls {
    /// Checks the PEM documents parse and the certificate and key are paired.
    pub fn validate(&self) -> Result<()> {
        if let Some(ca) = &self.ca_pem {
            ensure!(
                pem_certificates(ca, "tlsCaPem")? > 0,
                "tlsCaPem contains no certificate"
            );
        }
        match (&self.certificate_pem, &self.private_key_pem) {
            (None, None) => {}
            (Some(certificate), Some(key)) => {
                ensure!(
                    pem_certificates(certificate, "tlsClientCertificatePem")? > 0,
                    "tlsClientCertificatePem contains no certificate"
                );
                ensure!(
                    key.expose().len() <= MAX_PEM_BYTES,
                    "tlsClientPrivateKeyPem exceeds 256 KiB"
                );
                rustls_pemfile::private_key(&mut key.expose().as_bytes())
                    .map_err(|_| anyhow::anyhow!("tlsClientPrivateKeyPem is not valid PEM"))?
                    .ok_or_else(|| {
                        anyhow::anyhow!("tlsClientPrivateKeyPem contains no private key")
                    })?;
            }
            _ => anyhow::bail!(
                "tlsClientCertificatePem and tlsClientPrivateKeyPem must be configured together"
            ),
        }
        Ok(())
    }

    /// Whether any setting is present.
    pub fn is_configured(&self) -> bool {
        self.ca_pem.is_some() || self.certificate_pem.is_some()
    }
}

#[cfg(feature = "sink")]
fn pem_certificates(pem: &str, field: &str) -> Result<usize> {
    ensure!(pem.len() <= MAX_PEM_BYTES, "{field} exceeds 256 KiB");
    let mut count = 0;
    for certificate in rustls_pemfile::certs(&mut pem.as_bytes()) {
        certificate.map_err(|_| anyhow::anyhow!("{field} is not valid PEM"))?;
        count += 1;
    }
    Ok(count)
}

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
            !RESERVED_HEADERS.contains(&name.as_str()),
            "{name} is supplied automatically and may not be configured"
        );
        ensure!(
            result.insert(name.to_string(), value.clone()).is_none(),
            "duplicate header name ignoring case"
        );
    }
    Ok(result)
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

#[cfg(all(test, feature = "sink"))]
mod sink_tests {
    use super::*;

    #[test]
    fn sanitize_escapes_control_characters_and_caps_length() {
        assert_eq!(sanitize("a\nb"), "a\\nb");
        let long = "x".repeat(MAX_LOG_FIELD_LEN * 2);
        let clean = sanitize(&long);
        assert!(clean.len() <= MAX_LOG_FIELD_LEN + 3 && clean.ends_with("..."));
        assert!(sanitize(&"é".repeat(400)).len() <= MAX_LOG_FIELD_LEN + 3);
    }

    #[test]
    fn client_tls_requires_paired_valid_pem() {
        assert!(ClientTls::default().validate().is_ok());
        assert!(ClientTls {
            ca_pem: Some("not pem".into()),
            ..Default::default()
        }
        .validate()
        .is_err());
        let key = rcgen::KeyPair::generate().expect("key");
        let certificate = rcgen::CertificateParams::new(vec!["localhost".into()])
            .expect("params")
            .self_signed(&key)
            .expect("certificate");
        assert!(ClientTls {
            certificate_pem: Some(certificate.pem()),
            ..Default::default()
        }
        .validate()
        .is_err());
        ClientTls {
            ca_pem: Some(certificate.pem()),
            certificate_pem: Some(certificate.pem()),
            private_key_pem: Some(SecretString::new(key.serialize_pem())),
        }
        .validate()
        .expect("valid client TLS");
    }
}
