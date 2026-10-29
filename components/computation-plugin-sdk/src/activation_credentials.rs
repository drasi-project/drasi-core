// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bounded, activation-only authentication data. Identity providers stay in the
//! host; these values must never become part of a component's configuration.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundCreateRequest {
    activation_credentials_version: u32,
    request: crate::CreateRequest,
}

/// Only use after negotiating the activation-credentials extension for the
/// factory. The ordinary CreateRequest encoding remains unchanged.
pub fn encode_bound_create_request(request: &crate::CreateRequest) -> anyhow::Result<Vec<u8>> {
    crate::wire::encode(&BoundCreateRequest {
        activation_credentials_version: crate::abi::activation_credentials::VERSION,
        request: request.clone(),
    })
}

pub(crate) fn decode_create_request(bytes: &[u8]) -> anyhow::Result<crate::CreateRequest> {
    if let Ok(request) = crate::wire::decode::<crate::CreateRequest>(bytes) {
        return Ok(request);
    }
    let mut bound: BoundCreateRequest = crate::wire::decode(bytes)
        .map_err(|_| anyhow::anyhow!("invalid native component creation request"))?;
    anyhow::ensure!(
        bound.activation_credentials_version == crate::abi::activation_credentials::VERSION,
        "unsupported activation credential creation version"
    );
    bound.request.activation_credentials_bound = true;
    Ok(bound.request)
}

pub use crate::abi::activation_credentials::{
    MAX_CREDENTIAL_FIELD_BYTES, MAX_CREDENTIAL_PAYLOAD_BYTES,
};

/// The component's configured endpoint, computed without I/O during construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationCredentialContext {
    pub hostname: String,
    pub port: u16,
}

impl ActivationCredentialContext {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.hostname.is_empty()
                && self.hostname.len() <= 4096
                && !self.hostname.contains('\0')
                && self.port != 0,
            "invalid activation credential endpoint"
        );
        Ok(())
    }
}

/// A single activation's credentials. Certificates are deliberately unsupported:
/// the host rejects them before invoking native startup.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ActivationCredentials {
    UsernamePassword { username: String, password: String },
    Token { username: String, token: String },
}

impl ActivationCredentials {
    pub fn validate(&self) -> anyhow::Result<()> {
        let (username, secret) = match self {
            Self::UsernamePassword { username, password } => (username, password),
            Self::Token { username, token } => (username, token),
        };
        anyhow::ensure!(
            username.len() <= MAX_CREDENTIAL_FIELD_BYTES
                && secret.len() <= MAX_CREDENTIAL_FIELD_BYTES,
            "activation credentials exceed the supported size"
        );
        Ok(())
    }

    pub fn into_auth_pair(self) -> (String, String) {
        match self {
            Self::UsernamePassword { username, password } => (username, password),
            Self::Token { username, token } => (username, token),
        }
    }
}

impl fmt::Debug for ActivationCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct(match self {
            Self::UsernamePassword { .. } => "UsernamePassword",
            Self::Token { .. } => "Token",
        })
        .field("username", &"[REDACTED]")
        .field("credential", &"[REDACTED]")
        .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, ImplementationIdentity};
    use serde_json::json;

    #[test]
    fn activation_credentials_creation_marker_is_never_in_base_wire_or_configuration(
    ) -> anyhow::Result<()> {
        let request = crate::CreateRequest {
            inputs: Vec::new(),
            id: ComponentId::try_new("component")?,
            implementation: ImplementationIdentity::try_new("fixture/credentials", "1")?,
            configuration_version: 1,
            configuration: json!({"hostname":"host", "port":3306, "user":""}),
            scope: None,
            activation_credentials_bound: true,
        };
        let json = serde_json::to_value(&request)?;
        assert!(json.get("activation_credentials_bound").is_none());
        assert_eq!(json.as_object().unwrap().len(), 5);
        let base = decode_create_request(&crate::wire::encode(&request)?)?;
        assert!(!base.activation_credentials_bound);
        assert_eq!(base.configuration, request.configuration);
        let bound = decode_create_request(&encode_bound_create_request(&request)?)?;
        assert!(bound.activation_credentials_bound);
        assert_eq!(bound.configuration, request.configuration);
        assert_eq!(serde_json::to_value(bound)?, json);
        let mut invalid = json;
        invalid["activation_credentials_bound"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<crate::CreateRequest>(invalid).is_err());
        let unsupported = crate::wire::encode(&BoundCreateRequest {
            activation_credentials_version: 2,
            request,
        })?;
        assert!(decode_create_request(&unsupported).is_err());
        Ok(())
    }

    #[test]
    fn activation_credentials_are_redacted_bounded_and_preserve_kind() -> anyhow::Result<()> {
        for credential in [
            ActivationCredentials::UsernamePassword {
                username: "secret-user".into(),
                password: "secret-value".into(),
            },
            ActivationCredentials::Token {
                username: "secret-user".into(),
                token: "secret-value".into(),
            },
        ] {
            credential.validate()?;
            let debug = format!("{credential:?}");
            assert!(!debug.contains("secret-user") && !debug.contains("secret-value"));
            assert_eq!(
                crate::wire::decode::<ActivationCredentials>(&crate::wire::encode(&credential)?)?,
                credential
            );
        }
        assert!(ActivationCredentials::Token {
            username: String::new(),
            token: "x".repeat(MAX_CREDENTIAL_FIELD_BYTES + 1)
        }
        .validate()
        .is_err());
        assert!(ActivationCredentialContext {
            hostname: "host".into(),
            port: 0
        }
        .validate()
        .is_err());
        assert!(ActivationCredentialContext {
            hostname: String::new(),
            port: 1
        }
        .validate()
        .is_err());
        Ok(())
    }
}
