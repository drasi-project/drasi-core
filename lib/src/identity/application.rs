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

//! In-process identity provider that delegates credential acquisition to a
//! host-supplied closure.
//!
//! This is the identity-provider counterpart to `ApplicationSource`,
//! `ApplicationReaction`, and `ApplicationBootstrapProvider`: it lets a host
//! application reuse its existing authentication code (Azure AD, AWS, Vault,
//! etc.) instead of configuring a separate Drasi identity-provider plugin.

use super::{CredentialContext, Credentials, IdentityProvider};
use anyhow::Result;
use async_trait::async_trait;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Boxed async credential callback that a user-supplied closure is stored as.
type AsyncCredentialCallback = dyn Fn(&CredentialContext) -> Pin<Box<dyn Future<Output = Result<Credentials>> + Send>>
    + Send
    + Sync;

/// Identity provider whose `get_credentials` calls a host-supplied closure.
///
/// Use [`ApplicationIdentityProvider::new`] for an async closure (typical for
/// real token-acquisition flows) or [`ApplicationIdentityProvider::new_sync`]
/// for a synchronous closure (handy for tests and static credentials).
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use drasi_lib::identity::{ApplicationIdentityProvider, Credentials};
///
/// let provider = ApplicationIdentityProvider::new(|ctx| {
///     let host = ctx.get("hostname").unwrap_or("default").to_string();
///     async move {
///         // call into your existing auth code here
///         Ok(Credentials::UsernamePassword {
///             username: format!("user@{host}"),
///             password: "secret".into(),
///         })
///     }
/// });
/// let _provider: Arc<dyn drasi_lib::identity::IdentityProvider> = Arc::new(provider);
/// ```
#[derive(Clone)]
pub struct ApplicationIdentityProvider {
    callback: Arc<AsyncCredentialCallback>,
}

impl ApplicationIdentityProvider {
    /// Create a provider backed by an async closure.
    ///
    /// See also [`ApplicationIdentityProvider::new_sync`] for synchronous
    /// callbacks.
    pub fn new<F, Fut>(callback: F) -> Self
    where
        F: Fn(&CredentialContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Credentials>> + Send + 'static,
    {
        let cb: Arc<AsyncCredentialCallback> =
            Arc::new(move |ctx| Box::pin(callback(ctx)) as Pin<Box<_>>);
        Self { callback: cb }
    }

    /// Create a provider backed by a synchronous closure.
    ///
    /// Convenience wrapper for callbacks that don't need to await — the
    /// closure result is wrapped in a ready future.
    ///
    /// See also [`ApplicationIdentityProvider::new`] for async callbacks.
    pub fn new_sync<F>(callback: F) -> Self
    where
        F: Fn(&CredentialContext) -> Result<Credentials> + Send + Sync + 'static,
    {
        let cb: Arc<AsyncCredentialCallback> = Arc::new(move |ctx| {
            let result = callback(ctx);
            Box::pin(async move { result })
        });
        Self { callback: cb }
    }
}

impl std::fmt::Debug for ApplicationIdentityProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApplicationIdentityProvider")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl IdentityProvider for ApplicationIdentityProvider {
    async fn get_credentials(&self, context: &CredentialContext) -> Result<Credentials> {
        (self.callback)(context).await
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn sync_closure_returns_username_password() {
        let provider = ApplicationIdentityProvider::new_sync(|_ctx| {
            Ok(Credentials::UsernamePassword {
                username: "alice".into(),
                password: "s3cret".into(),
            })
        });

        let creds = provider
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap();

        assert_eq!(
            creds,
            Credentials::UsernamePassword {
                username: "alice".into(),
                password: "s3cret".into(),
            }
        );
    }

    #[tokio::test]
    async fn async_closure_returns_token() {
        let provider = ApplicationIdentityProvider::new(|_ctx| async {
            Ok(Credentials::Token {
                username: "svc".into(),
                token: "abc.def.ghi".into(),
            })
        });

        let creds = provider
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap();

        assert_eq!(
            creds,
            Credentials::Token {
                username: "svc".into(),
                token: "abc.def.ghi".into(),
            }
        );
    }

    #[tokio::test]
    async fn callback_observes_credential_context() {
        let provider = ApplicationIdentityProvider::new_sync(|ctx| {
            let host = ctx.get("hostname").unwrap_or("none").to_string();
            let port = ctx.get("port").unwrap_or("0").to_string();
            Ok(Credentials::UsernamePassword {
                username: format!("user@{host}:{port}"),
                password: "pw".into(),
            })
        });

        let ctx = CredentialContext::new()
            .with_property("hostname", "db.example.com")
            .with_property("port", "5432");

        let creds = provider.get_credentials(&ctx).await.unwrap();

        assert_eq!(
            creds,
            Credentials::UsernamePassword {
                username: "user@db.example.com:5432".into(),
                password: "pw".into(),
            }
        );
    }

    #[tokio::test]
    async fn clone_box_shares_underlying_callback() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_cb = calls.clone();

        let provider = ApplicationIdentityProvider::new_sync(move |_ctx| {
            calls_for_cb.fetch_add(1, Ordering::SeqCst);
            Ok(Credentials::UsernamePassword {
                username: "u".into(),
                password: "p".into(),
            })
        });

        let cloned = provider.clone_box();

        provider
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap();
        cloned
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn callback_error_is_propagated() {
        let provider = ApplicationIdentityProvider::new_sync(|_ctx| {
            Err(anyhow::anyhow!("auth backend unavailable"))
        });

        let err = provider
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap_err();

        assert!(err.to_string().contains("auth backend unavailable"));
    }

    #[tokio::test]
    async fn sync_closure_returns_certificate() {
        let provider = ApplicationIdentityProvider::new_sync(|_ctx| {
            Ok(Credentials::Certificate {
                cert_pem: "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----".into(),
                key_pem: "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----".into(),
                username: Some("cert-user".into()),
            })
        });

        let creds = provider
            .get_credentials(&CredentialContext::new())
            .await
            .unwrap();

        assert!(creds.is_certificate());
        assert_eq!(
            creds,
            Credentials::Certificate {
                cert_pem: "-----BEGIN CERTIFICATE-----\nMIIB...\n-----END CERTIFICATE-----".into(),
                key_pem: "-----BEGIN PRIVATE KEY-----\nMIIE...\n-----END PRIVATE KEY-----".into(),
                username: Some("cert-user".into()),
            }
        );
    }

    #[tokio::test]
    async fn debug_impl_does_not_leak_callback_state() {
        let provider = ApplicationIdentityProvider::new_sync(|_ctx| {
            Ok(Credentials::UsernamePassword {
                username: "should-not-appear".into(),
                password: "super-secret".into(),
            })
        });

        let formatted = format!("{provider:?}");

        assert!(formatted.contains("ApplicationIdentityProvider"));
        // The Debug impl is intentionally opaque — it must not surface
        // closure-captured state or anything the closure might return.
        assert!(!formatted.contains("super-secret"));
        assert!(!formatted.contains("should-not-appear"));
    }
}
