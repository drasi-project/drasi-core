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

/// Trait for identity providers that supply authentication credentials.
///
/// This is a plugin trait (Layer 3) — implementations return `anyhow::Result`
/// and should use `.context()` for error chains. The framework wraps these
/// into `DrasiError` at the public API boundary.
#[async_trait]
pub trait IdentityProvider: Send + Sync {
    /// Fetch credentials for authentication.
    async fn get_credentials(&self) -> Result<Credentials>;

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

// Manual Debug impl to redact sensitive fields (passwords, tokens, keys)
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credentials::UsernamePassword { username, .. } => f
                .debug_struct("UsernamePassword")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .finish(),
            Credentials::Token { username, .. } => f
                .debug_struct("Token")
                .field("username", username)
                .field("token", &"[REDACTED]")
                .finish(),
            Credentials::Certificate { username, .. } => f
                .debug_struct("Certificate")
                .field("cert_pem", &"[REDACTED]")
                .field("key_pem", &"[REDACTED]")
                .field("username", username)
                .finish(),
        }
    }
}

impl Credentials {
    /// Extract username and password/token for connection string building.
    ///
    /// Returns `Err(self)` if this is a `Certificate` variant.
    pub fn try_into_auth_pair(self) -> std::result::Result<(String, String), Self> {
        match self {
            Credentials::UsernamePassword { username, password } => Ok((username, password)),
            Credentials::Token { username, token } => Ok((username, token)),
            other => Err(other),
        }
    }

    /// Extract certificate and key for TLS client authentication.
    ///
    /// Returns `Ok((cert_pem, key_pem, optional_username))` for `Certificate` credentials,
    /// or `Err(self)` for other variants.
    pub fn try_into_certificate(
        self,
    ) -> std::result::Result<(String, String, Option<String>), Self> {
        match self {
            Credentials::Certificate {
                cert_pem,
                key_pem,
                username,
            } => Ok((cert_pem, key_pem, username)),
            other => Err(other),
        }
    }

    /// Extract username and password/token for connection string building.
    ///
    /// # Panics
    /// Panics if called on `Certificate` credentials.
    ///
    /// # Deprecated
    /// Use [`try_into_auth_pair`](Self::try_into_auth_pair) instead.
    #[deprecated(note = "Use try_into_auth_pair() which returns Result instead of panicking")]
    pub fn into_auth_pair(self) -> (String, String) {
        self.try_into_auth_pair()
            .unwrap_or_else(|_| panic!("Certificate credentials cannot be converted to an auth pair. Use try_into_auth_pair() or try_into_certificate() instead."))
    }

    /// Extract certificate and key for TLS client authentication.
    ///
    /// # Panics
    /// Panics if called on non-Certificate credentials.
    ///
    /// # Deprecated
    /// Use [`try_into_certificate`](Self::try_into_certificate) instead.
    #[deprecated(note = "Use try_into_certificate() which returns Result instead of panicking")]
    pub fn into_certificate(self) -> (String, String, Option<String>) {
        self.try_into_certificate()
            .unwrap_or_else(|_| panic!("Not certificate credentials. Use try_into_certificate() or try_into_auth_pair() instead."))
    }

    /// Returns `true` if this is a `Certificate` variant.
    pub fn is_certificate(&self) -> bool {
        matches!(self, Credentials::Certificate { .. })
    }
}

mod password;
pub use password::PasswordIdentityProvider;

#[cfg(feature = "azure-identity")]
mod azure;
#[cfg(feature = "azure-identity")]
pub use azure::AzureIdentityProvider;

#[cfg(feature = "aws-identity")]
mod aws;
#[cfg(feature = "aws-identity")]
pub use aws::AwsIdentityProvider;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_password_provider() {
        let provider = PasswordIdentityProvider::new("testuser", "testpass");
        let credentials = provider.get_credentials().await.unwrap();

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

        let credentials = cloned.get_credentials().await.unwrap();
        assert!(matches!(credentials, Credentials::UsernamePassword { .. }));
    }

    #[test]
    fn test_try_into_auth_pair_username_password() {
        let creds = Credentials::UsernamePassword {
            username: "user".into(),
            password: "pass".into(),
        };
        let (u, p) = creds.try_into_auth_pair().unwrap();
        assert_eq!(u, "user");
        assert_eq!(p, "pass");
    }

    #[test]
    fn test_try_into_auth_pair_token() {
        let creds = Credentials::Token {
            username: "user".into(),
            token: "tok".into(),
        };
        let (u, t) = creds.try_into_auth_pair().unwrap();
        assert_eq!(u, "user");
        assert_eq!(t, "tok");
    }

    #[test]
    fn test_try_into_auth_pair_rejects_certificate() {
        let creds = Credentials::Certificate {
            cert_pem: "cert".into(),
            key_pem: "key".into(),
            username: None,
        };
        let result = creds.try_into_auth_pair();
        assert!(result.is_err());
        // Verify the original credentials are returned in the Err
        let returned = result.unwrap_err();
        assert!(returned.is_certificate());
    }

    #[test]
    fn test_try_into_certificate_success() {
        let creds = Credentials::Certificate {
            cert_pem: "cert".into(),
            key_pem: "key".into(),
            username: Some("user".into()),
        };
        let (c, k, u) = creds.try_into_certificate().unwrap();
        assert_eq!(c, "cert");
        assert_eq!(k, "key");
        assert_eq!(u, Some("user".into()));
    }

    #[test]
    fn test_try_into_certificate_rejects_password() {
        let creds = Credentials::UsernamePassword {
            username: "user".into(),
            password: "pass".into(),
        };
        let result = creds.try_into_certificate();
        assert!(result.is_err());
    }
}
