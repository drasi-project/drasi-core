// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Listener access control: bearer-token authentication, per-client rate
//! limiting and TLS server configuration.

use crate::config::{RateLimit, SecretString, TlsMaterial, MAX_PEM_BYTES};
use anyhow::{ensure, Context, Result};
use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::Instant,
};
use subtle::ConstantTimeEq;

/// Validates `Authorization: Bearer <token>` against the configured tokens in
/// constant time with respect to token contents.
#[derive(Clone, Debug, Default)]
pub struct Authenticator {
    tokens: Vec<SecretString>,
}

impl Authenticator {
    pub fn new(tokens: Vec<SecretString>) -> Self {
        Self { tokens }
    }

    /// Whether any token is configured; without one every request is accepted.
    pub fn enabled(&self) -> bool {
        !self.tokens.is_empty()
    }

    /// Checks an `Authorization` header value.
    pub fn authorize(&self, authorization: Option<&str>) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        let Some(presented) = authorization.and_then(|value| {
            let (scheme, token) = value.trim().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
        }) else {
            return false;
        };
        // Compare against every token so the number of comparisons does not
        // reveal which token matched.
        self.tokens.iter().fold(false, |matched, token| {
            matched | bool::from(token.expose().as_bytes().ct_eq(presented.as_bytes()))
        })
    }
}

/// Upper bound on tracked client addresses. When exceeded, idle (full) buckets
/// are discarded first; tracking never grows without bound.
const MAX_TRACKED_CLIENTS: usize = 65_536;

struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// Token-bucket rate limiter keyed by client IP address.
pub struct RateLimiter {
    limit: RateLimit,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RateLimiter")
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl RateLimiter {
    pub fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `client`; false when its budget is exhausted.
    pub fn check(&self, client: IpAddr) -> bool {
        self.check_at(client, Instant::now())
    }

    fn check_at(&self, client: IpAddr, now: Instant) -> bool {
        let rate = f64::from(self.limit.per_second);
        let burst = f64::from(self.limit.burst);
        let Ok(mut buckets) = self.buckets.lock() else {
            // A poisoned limiter fails closed rather than silently unlimited.
            return false;
        };
        if buckets.len() >= MAX_TRACKED_CLIENTS && !buckets.contains_key(&client) {
            buckets.retain(|_, bucket| {
                bucket.tokens + now.saturating_duration_since(bucket.updated).as_secs_f64() * rate
                    < burst
            });
            if buckets.len() >= MAX_TRACKED_CLIENTS {
                buckets.clear();
            }
        }
        let bucket = buckets.entry(client).or_insert(Bucket {
            tokens: burst,
            updated: now,
        });
        let refill = now.saturating_duration_since(bucket.updated).as_secs_f64() * rate;
        bucket.tokens = (bucket.tokens + refill).min(burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn certificates(pem: &str, field: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    ensure!(pem.len() <= MAX_PEM_BYTES, "{field} exceeds 256 KiB");
    let certificates = rustls_pemfile::certs(&mut pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("{field} is not valid PEM"))?;
    ensure!(!certificates.is_empty(), "{field} contains no certificate");
    Ok(certificates)
}

impl TlsMaterial {
    /// Parses every PEM document and checks that the key matches the leaf.
    pub fn validate(&self) -> Result<()> {
        self.server_config(Vec::new()).map(|_| ())
    }

    /// A rustls server configuration using the ring provider and safe default
    /// protocol versions. `alpn` lists the application protocols to offer.
    pub fn server_config(&self, alpn: Vec<Vec<u8>>) -> Result<Arc<rustls::ServerConfig>> {
        let chain = certificates(&self.certificate_pem, "tlsCertificatePem")?;
        let key_pem = self.private_key_pem.expose();
        ensure!(
            key_pem.len() <= MAX_PEM_BYTES,
            "tlsPrivateKeyPem exceeds 256 KiB"
        );
        let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())
            .context("tlsPrivateKeyPem is not valid PEM")?
            .context("tlsPrivateKeyPem contains no private key")?;
        let builder = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .context("TLS provider has no safe protocol versions")?;
        let builder = match &self.client_ca_pem {
            Some(pem) => {
                let mut roots = rustls::RootCertStore::empty();
                for certificate in certificates(pem, "tlsClientCaPem")? {
                    roots
                        .add(certificate)
                        .context("tlsClientCaPem contains an invalid CA certificate")?;
                }
                let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots),
                    provider(),
                )
                .build()
                .context("tlsClientCaPem cannot verify client certificates")?;
                builder.with_client_cert_verifier(verifier)
            }
            None => builder.with_no_client_auth(),
        };
        let mut config = builder
            .with_single_cert(chain, key)
            .context("tlsPrivateKeyPem does not match tlsCertificatePem")?;
        config.alpn_protocols = alpn;
        Ok(Arc::new(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bearer_tokens_are_checked_exactly() {
        let auth = Authenticator::new(vec![
            SecretString::new("0123456789abcdef"),
            SecretString::new("fedcba9876543210"),
        ]);
        assert!(auth.authorize(Some("Bearer 0123456789abcdef")));
        assert!(auth.authorize(Some("bearer fedcba9876543210")));
        assert!(!auth.authorize(Some("Bearer 0123456789abcde")));
        assert!(!auth.authorize(Some("Basic 0123456789abcdef")));
        assert!(!auth.authorize(Some("0123456789abcdef")));
        assert!(!auth.authorize(None));
        assert!(Authenticator::default().authorize(None));
    }

    #[test]
    fn rate_limiter_refills_per_client() {
        let limiter = RateLimiter::new(RateLimit {
            per_second: 2,
            burst: 2,
        });
        let start = Instant::now();
        let client: IpAddr = "10.0.0.1".parse().expect("ip");
        let other: IpAddr = "10.0.0.2".parse().expect("ip");
        assert!(limiter.check_at(client, start));
        assert!(limiter.check_at(client, start));
        assert!(!limiter.check_at(client, start));
        assert!(limiter.check_at(other, start));
        assert!(limiter.check_at(client, start + Duration::from_millis(500)));
        assert!(!limiter.check_at(client, start + Duration::from_millis(500)));
    }

    fn certificate(name: &str) -> rcgen::CertifiedKey {
        rcgen::generate_simple_self_signed(vec![name.to_owned()]).expect("certificate")
    }

    #[test]
    fn valid_tls_material_builds_server_and_mutual_tls_configurations() {
        let server = certificate("localhost");
        let client_ca = certificate("client-ca");
        let material = TlsMaterial {
            certificate_pem: server.cert.pem(),
            private_key_pem: SecretString::new(server.key_pair.serialize_pem()),
            client_ca_pem: None,
        };
        let config = material
            .server_config(vec![b"h2".to_vec()])
            .expect("server TLS");
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec()]);
        let mutual = TlsMaterial {
            client_ca_pem: Some(client_ca.cert.pem()),
            ..material.clone()
        };
        mutual.validate().expect("mutual TLS");
        let mismatched = TlsMaterial {
            private_key_pem: SecretString::new(client_ca.key_pair.serialize_pem()),
            ..material
        };
        assert!(mismatched.validate().is_err(), "key must match certificate");
    }

    #[test]
    fn invalid_tls_material_is_rejected() {
        let material = TlsMaterial {
            certificate_pem: "not pem".into(),
            private_key_pem: SecretString::new("not pem"),
            client_ca_pem: None,
        };
        assert!(material.validate().is_err());
    }
}
