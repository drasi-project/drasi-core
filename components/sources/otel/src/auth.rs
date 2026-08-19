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

//! Inbound OTLP authentication.

use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use tonic::{Request, Status};

use crate::config::OtelSourceConfig;

/// Resolve the expected inbound credential, if any.
pub async fn expected_credentials(
    identity: Option<Arc<dyn IdentityProvider>>,
    config: &OtelSourceConfig,
) -> anyhow::Result<Option<ExpectedAuth>> {
    if let Some(provider) = identity {
        let creds = provider
            .get_credentials(&CredentialContext::default())
            .await
            .context("identity provider failed")?;
        return match creds {
            Credentials::Token { token, .. } => Ok(Some(ExpectedAuth::Bearer(token))),
            Credentials::UsernamePassword { username, password } => {
                Ok(Some(ExpectedAuth::Basic { username, password }))
            }
            Credentials::Certificate { .. } => Ok(None),
        };
    }
    if let Some(token) = &config.auth_token {
        return Ok(Some(ExpectedAuth::Bearer(token.clone())));
    }
    Ok(None)
}

/// Expected inbound credential.
#[derive(Clone)]
pub enum ExpectedAuth {
    Bearer(String),
    Basic { username: String, password: String },
}

impl std::fmt::Debug for ExpectedAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bearer(_) => f.debug_tuple("Bearer").field(&"[REDACTED]").finish(),
            Self::Basic { username, .. } => f
                .debug_struct("Basic")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .finish(),
        }
    }
}

/// Check gRPC metadata against the expected credential.
#[allow(clippy::result_large_err)]
pub fn authorize_grpc<T>(
    request: &Request<T>,
    expected: Option<&ExpectedAuth>,
) -> Result<(), Status> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let header = request
        .metadata()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if header_matches(header, expected) {
        Ok(())
    } else {
        Err(Status::unauthenticated("invalid authorization"))
    }
}

/// Check an HTTP Authorization header.
pub fn authorize_http(header: Option<&str>, expected: Option<&ExpectedAuth>) -> bool {
    match expected {
        None => true,
        Some(expected) => header_matches(header.unwrap_or(""), expected),
    }
}

fn header_matches(header: &str, expected: &ExpectedAuth) -> bool {
    match expected {
        ExpectedAuth::Bearer(token) => {
            let Some(value) = header
                .strip_prefix("Bearer ")
                .or_else(|| header.strip_prefix("bearer "))
            else {
                return false;
            };
            constant_time_eq(value.as_bytes(), token.as_bytes())
        }
        ExpectedAuth::Basic { username, password } => {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
            let basic = format!("Basic {encoded}");
            let basic_lc = format!("basic {encoded}");
            constant_time_eq(header.as_bytes(), basic.as_bytes())
                || constant_time_eq(header.as_bytes(), basic_lc.as_bytes())
        }
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if left.len() != right.len() {
        let _ = left.ct_eq(left);
        return false;
    }
    bool::from(left.ct_eq(right))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_matches() {
        let expected = ExpectedAuth::Bearer("s3cret".to_string());
        assert!(header_matches("Bearer s3cret", &expected));
        assert!(!header_matches("Bearer other", &expected));
        assert!(!header_matches("Bearer s3cretx", &expected));
    }

    #[test]
    fn debug_redacts_secrets() {
        let token = ExpectedAuth::Bearer("super-s3cret".to_string());
        assert!(!format!("{token:?}").contains("super-s3cret"));
    }

    #[test]
    fn bearer_rejects_token_without_prefix() {
        let expected = ExpectedAuth::Bearer("s3cret".to_string());
        assert!(!header_matches("s3cret", &expected));
    }
}
