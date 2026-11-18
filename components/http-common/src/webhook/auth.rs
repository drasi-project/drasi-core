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

//! Authentication module for webhook requests.
//!
//! Supports:
//! - HMAC signature verification (SHA1, SHA256) for GitHub, Shopify, etc.
//! - Bearer token verification
//!
//! Secrets are named by environment variable in configuration. They are
//! resolved once, when a configuration is accepted ([`ResolvedAuth::resolve`]),
//! so a missing secret is a configuration error and requests never read the
//! process environment.

use super::config::{
    AuthConfig, BearerConfig, SignatureAlgorithm, SignatureConfig, SignatureEncoding,
};
use anyhow::{anyhow, ensure, Result};
use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::Sha256;
use std::env;
use subtle::ConstantTimeEq;

/// Result of authentication verification
#[derive(Debug, Clone, PartialEq)]
pub enum AuthResult {
    /// Authentication succeeded
    Success,
    /// No authentication configured (pass-through)
    NotConfigured,
    /// Authentication failed with reason
    Failed(String),
}

impl AuthResult {
    /// Returns true if authentication passed or was not required
    pub fn is_ok(&self) -> bool {
        matches!(self, AuthResult::Success | AuthResult::NotConfigured)
    }
}

/// A route's authentication with its secrets resolved.
pub struct ResolvedAuth {
    signature: Option<(SignatureConfig, Secret)>,
    bearer: Option<Secret>,
}

/// A resolved secret value; never printed.
struct Secret(String);

impl std::fmt::Debug for ResolvedAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ResolvedAuth")
            .field(
                "signature",
                &self.signature.as_ref().map(|(config, _)| config),
            )
            .field("bearer", &self.bearer.is_some())
            .finish()
    }
}

fn resolve_secret(name: &str, purpose: &str) -> Result<Secret> {
    let value = env::var(name)
        .map_err(|_| anyhow!("environment variable {name} for the {purpose} is not set"))?;
    ensure!(
        !value.is_empty(),
        "environment variable {name} for the {purpose} is empty"
    );
    Ok(Secret(value))
}

impl ResolvedAuth {
    /// Reads every secret the configuration names. Fails when one is unset
    /// or empty.
    pub fn resolve(config: &AuthConfig) -> Result<Self> {
        Ok(Self {
            signature: config
                .signature
                .as_ref()
                .map(|signature| {
                    resolve_secret(&signature.secret_env, "signature secret")
                        .map(|secret| (signature.clone(), secret))
                })
                .transpose()?,
            bearer: config
                .bearer
                .as_ref()
                .map(|bearer: &BearerConfig| resolve_secret(&bearer.token_env, "bearer token"))
                .transpose()?,
        })
    }

    /// Verifies a request. If both signature and bearer token are
    /// configured, both must pass.
    pub fn verify(&self, headers: &axum::http::HeaderMap, body: &[u8]) -> AuthResult {
        if let Some((config, secret)) = &self.signature {
            if let Err(e) = verify_signature(config, secret, headers, body) {
                return AuthResult::Failed(format!("Signature verification failed: {e}"));
            }
        }
        if let Some(token) = &self.bearer {
            if let Err(e) = verify_bearer(token, headers) {
                return AuthResult::Failed(format!("Bearer token verification failed: {e}"));
            }
        }
        if self.signature.is_some() || self.bearer.is_some() {
            AuthResult::Success
        } else {
            AuthResult::NotConfigured
        }
    }
}

/// Resolves the configuration's secrets and verifies one request, reading the
/// environment on every call. Used by the ordinary (legacy) HTTP source; the
/// native source resolves once per configuration with [`ResolvedAuth`].
pub fn verify_auth(
    auth_config: Option<&AuthConfig>,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> AuthResult {
    let Some(config) = auth_config else {
        return AuthResult::NotConfigured;
    };
    match ResolvedAuth::resolve(config) {
        Ok(auth) => auth.verify(headers, body),
        Err(error) => AuthResult::Failed(error.to_string()),
    }
}

/// Verify HMAC signature
fn verify_signature(
    config: &SignatureConfig,
    secret: &Secret,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> Result<()> {
    let secret = &secret.0;

    // Get the signature from header
    let signature_header = headers
        .get(&config.header)
        .ok_or_else(|| anyhow!("Signature header '{}' not found", config.header))?
        .to_str()
        .map_err(|_| anyhow!("Invalid signature header value"))?;

    // Strip prefix if configured
    let signature_value = if let Some(ref prefix) = config.prefix {
        signature_header
            .strip_prefix(prefix)
            .ok_or_else(|| anyhow!("Signature header missing expected prefix '{prefix}'"))?
    } else {
        signature_header
    };

    // Decode the signature based on encoding
    let received_signature = match config.encoding {
        SignatureEncoding::Hex => {
            hex::decode(signature_value).map_err(|e| anyhow!("Invalid hex signature: {e}"))?
        }
        SignatureEncoding::Base64 => {
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, signature_value)
                .map_err(|e| anyhow!("Invalid base64 signature: {e}"))?
        }
    };

    // Compute expected signature
    let expected_signature = match config.algorithm {
        SignatureAlgorithm::HmacSha1 => compute_hmac_sha1(secret.as_bytes(), body)?,
        SignatureAlgorithm::HmacSha256 => compute_hmac_sha256(secret.as_bytes(), body)?,
    };

    // Constant-time comparison
    if constant_time_compare(&received_signature, &expected_signature) {
        Ok(())
    } else {
        Err(anyhow!("Signature mismatch"))
    }
}

/// Compute HMAC-SHA1
fn compute_hmac_sha1(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac =
        Hmac::<Sha1>::new_from_slice(key).map_err(|e| anyhow!("HMAC-SHA1 key error: {e}"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// Compute HMAC-SHA256
fn compute_hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(key).map_err(|e| anyhow!("HMAC-SHA256 key error: {e}"))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// Constant-time comparison to prevent timing attacks
fn constant_time_compare(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Verify bearer token
fn verify_bearer(expected: &Secret, headers: &axum::http::HeaderMap) -> Result<()> {
    let expected_token = &expected.0;

    // Get the Authorization header
    let auth_header = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| anyhow!("Authorization header not found"))?
        .to_str()
        .map_err(|_| anyhow!("Invalid Authorization header value"))?;

    // Extract bearer token
    let received_token = auth_header
        .strip_prefix("Bearer ")
        .or_else(|| auth_header.strip_prefix("bearer "))
        .ok_or_else(|| anyhow!("Authorization header is not a Bearer token"))?;

    // Constant-time comparison
    if constant_time_compare(received_token.as_bytes(), expected_token.as_bytes()) {
        Ok(())
    } else {
        Err(anyhow!("Bearer token mismatch"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn create_headers(headers: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn secrets_are_resolved_once_and_missing_ones_are_refused() {
        let config = AuthConfig {
            signature: None,
            bearer: Some(BearerConfig {
                token_env: "TEST_RESOLVED_ONCE_TOKEN".to_string(),
            }),
        };
        assert!(ResolvedAuth::resolve(&config).is_err(), "unset secret");
        env::set_var("TEST_RESOLVED_ONCE_TOKEN", "first-token");
        let auth = ResolvedAuth::resolve(&config).unwrap();
        env::set_var("TEST_RESOLVED_ONCE_TOKEN", "second-token");
        let headers = create_headers(&[("authorization", "Bearer first-token")]);
        assert_eq!(auth.verify(&headers, b""), AuthResult::Success);
        env::remove_var("TEST_RESOLVED_ONCE_TOKEN");
    }

    #[test]
    fn test_auth_not_configured() {
        let headers = HeaderMap::new();
        let result = verify_auth(None, &headers, b"body");
        assert_eq!(result, AuthResult::NotConfigured);
    }

    #[test]
    fn test_auth_empty_config() {
        let config = AuthConfig {
            signature: None,
            bearer: None,
        };
        let headers = HeaderMap::new();
        let result = verify_auth(Some(&config), &headers, b"body");
        assert_eq!(result, AuthResult::NotConfigured);
    }

    #[test]
    fn test_hmac_sha256_github_style() {
        // Set up environment variable
        env::set_var("TEST_GITHUB_SECRET", "test-secret");

        let body = b"test payload";

        // Compute expected signature
        let expected_sig = compute_hmac_sha256(b"test-secret", body).unwrap();
        let sig_hex = hex::encode(&expected_sig);
        let sig_header = format!("sha256={sig_hex}");

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha256,
                secret_env: "TEST_GITHUB_SECRET".to_string(),
                header: "X-Hub-Signature-256".to_string(),
                prefix: Some("sha256=".to_string()),
                encoding: SignatureEncoding::Hex,
            }),
            bearer: None,
        };

        let headers = create_headers(&[("X-Hub-Signature-256", &sig_header)]);
        let result = verify_auth(Some(&config), &headers, body);
        assert_eq!(result, AuthResult::Success);

        env::remove_var("TEST_GITHUB_SECRET");
    }

    #[test]
    fn test_hmac_sha256_base64_shopify_style() {
        // Set up environment variable
        env::set_var("TEST_SHOPIFY_SECRET", "shopify-secret");

        let body = b"order data";

        // Compute expected signature
        let expected_sig = compute_hmac_sha256(b"shopify-secret", body).unwrap();
        let sig_base64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &expected_sig);

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha256,
                secret_env: "TEST_SHOPIFY_SECRET".to_string(),
                header: "X-Shopify-Hmac-Sha256".to_string(),
                prefix: None,
                encoding: SignatureEncoding::Base64,
            }),
            bearer: None,
        };

        let headers = create_headers(&[("X-Shopify-Hmac-Sha256", &sig_base64)]);
        let result = verify_auth(Some(&config), &headers, body);
        assert_eq!(result, AuthResult::Success);

        env::remove_var("TEST_SHOPIFY_SECRET");
    }

    #[test]
    fn test_hmac_sha1() {
        env::set_var("TEST_SHA1_SECRET", "sha1-secret");

        let body = b"test data";
        let expected_sig = compute_hmac_sha1(b"sha1-secret", body).unwrap();
        let sig_hex = hex::encode(&expected_sig);
        let sig_header = format!("sha1={sig_hex}");

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha1,
                secret_env: "TEST_SHA1_SECRET".to_string(),
                header: "X-Signature".to_string(),
                prefix: Some("sha1=".to_string()),
                encoding: SignatureEncoding::Hex,
            }),
            bearer: None,
        };

        let headers = create_headers(&[("X-Signature", &sig_header)]);
        let result = verify_auth(Some(&config), &headers, body);
        assert_eq!(result, AuthResult::Success);

        env::remove_var("TEST_SHA1_SECRET");
    }

    #[test]
    fn test_signature_mismatch() {
        env::set_var("TEST_SECRET_MISMATCH", "correct-secret");

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha256,
                secret_env: "TEST_SECRET_MISMATCH".to_string(),
                header: "X-Signature".to_string(),
                prefix: None,
                encoding: SignatureEncoding::Hex,
            }),
            bearer: None,
        };

        // Use wrong signature
        let headers = create_headers(&[(
            "X-Signature",
            "0000000000000000000000000000000000000000000000000000000000000000",
        )]);
        let result = verify_auth(Some(&config), &headers, b"body");

        match result {
            AuthResult::Failed(msg) => assert!(msg.contains("mismatch")),
            _ => panic!("Expected AuthResult::Failed"),
        }

        env::remove_var("TEST_SECRET_MISMATCH");
    }

    #[test]
    fn test_missing_signature_header() {
        env::set_var("TEST_SECRET_MISSING", "secret");

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha256,
                secret_env: "TEST_SECRET_MISSING".to_string(),
                header: "X-Signature".to_string(),
                prefix: None,
                encoding: SignatureEncoding::Hex,
            }),
            bearer: None,
        };

        let headers = HeaderMap::new();
        let result = verify_auth(Some(&config), &headers, b"body");

        match result {
            AuthResult::Failed(msg) => assert!(msg.contains("not found")),
            _ => panic!("Expected AuthResult::Failed"),
        }

        env::remove_var("TEST_SECRET_MISSING");
    }

    #[test]
    fn test_bearer_token_success() {
        env::set_var("TEST_BEARER_TOKEN", "my-secret-token");

        let config = AuthConfig {
            signature: None,
            bearer: Some(BearerConfig {
                token_env: "TEST_BEARER_TOKEN".to_string(),
            }),
        };

        let headers = create_headers(&[("authorization", "Bearer my-secret-token")]);
        let result = verify_auth(Some(&config), &headers, b"body");
        assert_eq!(result, AuthResult::Success);

        env::remove_var("TEST_BEARER_TOKEN");
    }

    #[test]
    fn test_bearer_token_mismatch() {
        env::set_var("TEST_BEARER_MISMATCH", "correct-token");

        let config = AuthConfig {
            signature: None,
            bearer: Some(BearerConfig {
                token_env: "TEST_BEARER_MISMATCH".to_string(),
            }),
        };

        let headers = create_headers(&[("authorization", "Bearer wrong-token")]);
        let result = verify_auth(Some(&config), &headers, b"body");

        match result {
            AuthResult::Failed(msg) => assert!(msg.contains("mismatch")),
            _ => panic!("Expected AuthResult::Failed"),
        }

        env::remove_var("TEST_BEARER_MISMATCH");
    }

    #[test]
    fn test_missing_bearer_header() {
        env::set_var("TEST_BEARER_MISSING", "token");

        let config = AuthConfig {
            signature: None,
            bearer: Some(BearerConfig {
                token_env: "TEST_BEARER_MISSING".to_string(),
            }),
        };

        let headers = HeaderMap::new();
        let result = verify_auth(Some(&config), &headers, b"body");

        match result {
            AuthResult::Failed(msg) => assert!(msg.contains("not found")),
            _ => panic!("Expected AuthResult::Failed"),
        }

        env::remove_var("TEST_BEARER_MISSING");
    }

    #[test]
    fn test_both_signature_and_bearer() {
        env::set_var("TEST_BOTH_SECRET", "sig-secret");
        env::set_var("TEST_BOTH_TOKEN", "bearer-token");

        let body = b"body";
        let expected_sig = compute_hmac_sha256(b"sig-secret", body).unwrap();
        let sig_hex = hex::encode(&expected_sig);

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha256,
                secret_env: "TEST_BOTH_SECRET".to_string(),
                header: "X-Signature".to_string(),
                prefix: None,
                encoding: SignatureEncoding::Hex,
            }),
            bearer: Some(BearerConfig {
                token_env: "TEST_BOTH_TOKEN".to_string(),
            }),
        };

        let headers = create_headers(&[
            ("X-Signature", &sig_hex),
            ("authorization", "Bearer bearer-token"),
        ]);
        let result = verify_auth(Some(&config), &headers, body);
        assert_eq!(result, AuthResult::Success);

        env::remove_var("TEST_BOTH_SECRET");
        env::remove_var("TEST_BOTH_TOKEN");
    }

    #[test]
    fn test_both_auth_signature_fails() {
        env::set_var("TEST_BOTH_SIG_FAIL_SECRET", "sig-secret");
        env::set_var("TEST_BOTH_SIG_FAIL_TOKEN", "bearer-token");

        let config = AuthConfig {
            signature: Some(SignatureConfig {
                algorithm: SignatureAlgorithm::HmacSha256,
                secret_env: "TEST_BOTH_SIG_FAIL_SECRET".to_string(),
                header: "X-Signature".to_string(),
                prefix: None,
                encoding: SignatureEncoding::Hex,
            }),
            bearer: Some(BearerConfig {
                token_env: "TEST_BOTH_SIG_FAIL_TOKEN".to_string(),
            }),
        };

        // Correct bearer but wrong signature
        let headers = create_headers(&[
            (
                "X-Signature",
                "0000000000000000000000000000000000000000000000000000000000000000",
            ),
            ("authorization", "Bearer bearer-token"),
        ]);
        let result = verify_auth(Some(&config), &headers, b"body");

        match result {
            AuthResult::Failed(msg) => assert!(msg.contains("Signature")),
            _ => panic!("Expected AuthResult::Failed"),
        }

        env::remove_var("TEST_BOTH_SIG_FAIL_SECRET");
        env::remove_var("TEST_BOTH_SIG_FAIL_TOKEN");
    }

    #[test]
    fn test_constant_time_compare() {
        assert!(constant_time_compare(b"hello", b"hello"));
        assert!(!constant_time_compare(b"hello", b"world"));
        assert!(!constant_time_compare(b"hello", b"hell"));
        assert!(!constant_time_compare(b"", b"a"));
        assert!(constant_time_compare(b"", b""));
    }
}
