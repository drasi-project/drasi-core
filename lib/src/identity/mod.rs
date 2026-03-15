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

//! Identity providers for authentication credentials.

use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

/// Context information that callers provide to identity providers.
///
/// This allows identity providers to generate context-specific credentials
/// (e.g., endpoint-specific tokens) without coupling their configuration
/// to a particular resource type.
///
/// # Common Properties
///
/// | Key        | Description                         | Example                            |
/// |------------|-------------------------------------|------------------------------------|
/// | `hostname` | Target endpoint hostname            | `"mydb.rds.amazonaws.com"`          |
/// | `port`     | Target endpoint port                | `"5432"`                            |
/// | `database` | Target database name                | `"mydb"`                            |
#[derive(Debug, Clone, Default)]
pub struct CredentialContext {
    /// Key-value properties that the identity provider may use.
    pub properties: HashMap<String, String>,
}

impl CredentialContext {
    /// Create a new empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a property on the context, returning self for chaining.
    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Get a property value.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(|s| s.as_str())
    }
}

/// Trait for identity providers that supply authentication credentials.
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// Fetch credentials for authentication.
    ///
    /// The `context` parameter provides optional caller-specific information
    /// (such as target hostname/port) that the provider may use to generate
    /// context-specific credentials. Providers that don't need this context
    /// can safely ignore it.
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials>;

    /// Clone the provider into a boxed trait object.
    fn clone_box(&self) -> Box<dyn IdentityProvider>;
}

impl Clone for Box<dyn IdentityProvider> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}

/// Credentials returned by an identity provider.
#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    /// Traditional username and password authentication.
    UsernamePassword { username: String, password: String },
    /// Token-based authentication (Azure AD, AWS IAM, etc.).
    Token { username: String, token: String },
    /// Client certificate authentication (mTLS).
    ///
    /// Used for database connections that authenticate via TLS client certificates
    /// instead of passwords or tokens.
    Certificate {
        /// PEM-encoded client certificate.
        cert_pem: String,
        /// PEM-encoded private key.
        key_pem: String,
        /// Optional username (some databases require it alongside certificates).
        username: Option<String>,
    },
}

impl Credentials {
    /// Extract username and password/token for connection string building.
    ///
    /// # Panics
    /// Panics if called on `Certificate` credentials. Use [`into_certificate`](Self::into_certificate)
    /// for certificate-based authentication.
    pub fn into_auth_pair(self) -> (String, String) {
        match self {
            Credentials::UsernamePassword { username, password } => (username, password),
            Credentials::Token { username, token } => (username, token),
            Credentials::Certificate { .. } => {
                panic!("Certificate credentials cannot be converted to an auth pair. Use into_certificate() instead.")
            }
        }
    }

    /// Extract certificate and key for TLS client authentication.
    ///
    /// Returns `(cert_pem, key_pem, optional_username)`.
    ///
    /// # Panics
    /// Panics if called on non-Certificate credentials.
    pub fn into_certificate(self) -> (String, String, Option<String>) {
        match self {
            Credentials::Certificate {
                cert_pem,
                key_pem,
                username,
            } => (cert_pem, key_pem, username),
            _ => panic!("Not certificate credentials. Use into_auth_pair() instead."),
        }
    }

    /// Returns `true` if this is a `Certificate` variant.
    pub fn is_certificate(&self) -> bool {
        matches!(self, Credentials::Certificate { .. })
    }
}

mod password;
pub use password::PasswordIdentityProvider;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_password_provider() {
        let provider = PasswordIdentityProvider::new("testuser", "testpass");
        let credentials = provider
            .get_credentials(&CredentialContext::default())
            .await
            .unwrap();

        match credentials {
            Credentials::UsernamePassword { username, password } => {
                assert_eq!(username, "testuser");
                assert_eq!(password, "testpass");
            }
            _ => panic!("Expected UsernamePassword credentials"),
        }
    }

    #[tokio::test]
    async fn test_provider_clone() {
        let provider: Box<dyn IdentityProvider> =
            Box::new(PasswordIdentityProvider::new("user", "pass"));
        let cloned = provider.clone();

        let credentials = cloned
            .get_credentials(&CredentialContext::default())
            .await
            .unwrap();
        assert!(matches!(credentials, Credentials::UsernamePassword { .. }));
    }
}
