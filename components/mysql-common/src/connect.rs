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

//! Shared MySQL connection helper with SSL-mode handling.
//!
//! Both the MySQL source and bootstrap plugins connect with the same
//! [`SslMode`] semantics. TLS is only attempted when this crate is built with
//! the `tls` feature (which pulls in a `mysql_async` TLS backend). Without that
//! feature, `mysql_async` has no TLS backend compiled in and would *panic*
//! (rather than return an error) if asked for a TLS connection — so the modes
//! that request TLS are handled defensively here instead of reaching that path.

use anyhow::Result;
use mysql_async::{Conn, OptsBuilder};

use crate::config::SslMode;

/// Connect to MySQL honoring the requested [`SslMode`].
///
/// `build_opts` must return a fresh [`OptsBuilder`] each time it is called
/// (without any `ssl_opts` set); this function applies the appropriate SSL
/// options based on `mode`. A closure is used because [`OptsBuilder`] is not
/// `Clone`, and [`SslMode::IfAvailable`] may need to build the options twice
/// (once for the TLS attempt and once for the plaintext fallback).
///
/// # Errors
///
/// Returns an error if the connection cannot be established, or if a TLS-
/// requiring mode ([`SslMode::Require`], [`SslMode::RequireVerifyCa`],
/// [`SslMode::RequireVerifyFull`]) is used while the crate was built without
/// the `tls` feature.
pub async fn connect_with_ssl_mode<F>(build_opts: F, mode: SslMode) -> Result<Conn>
where
    F: Fn() -> OptsBuilder,
{
    match mode {
        SslMode::Disabled => Ok(Conn::new(build_opts().ssl_opts(None)).await?),
        SslMode::IfAvailable => connect_if_available(build_opts).await,
        SslMode::Require => connect_require(build_opts, RequireMode::Relaxed).await,
        SslMode::RequireVerifyCa => connect_require(build_opts, RequireMode::VerifyCa).await,
        SslMode::RequireVerifyFull => connect_require(build_opts, RequireMode::VerifyFull).await,
    }
}

#[cfg(feature = "tls")]
async fn connect_if_available<F>(build_opts: F) -> Result<Conn>
where
    F: Fn() -> OptsBuilder,
{
    match Conn::new(build_opts().ssl_opts(Some(relaxed_ssl_opts()))).await {
        Ok(conn) => Ok(conn),
        Err(ssl_error) => {
            log::warn!(
                "SSL connection attempt failed, retrying without SSL: {ssl_error}"
            );
            Ok(Conn::new(build_opts().ssl_opts(None)).await?)
        }
    }
}

#[cfg(not(feature = "tls"))]
async fn connect_if_available<F>(build_opts: F) -> Result<Conn>
where
    F: Fn() -> OptsBuilder,
{
    // No TLS backend is compiled in; requesting SSL would panic inside
    // `mysql_async`. Connect in plaintext directly.
    Ok(Conn::new(build_opts().ssl_opts(None)).await?)
}

#[cfg_attr(not(feature = "tls"), allow(dead_code))]
enum RequireMode {
    Relaxed,
    VerifyCa,
    VerifyFull,
}

#[cfg(feature = "tls")]
async fn connect_require<F>(build_opts: F, require_mode: RequireMode) -> Result<Conn>
where
    F: Fn() -> OptsBuilder,
{
    let ssl_opts = match require_mode {
        RequireMode::Relaxed => relaxed_ssl_opts(),
        RequireMode::VerifyCa => verify_ca_ssl_opts(),
        RequireMode::VerifyFull => mysql_async::SslOpts::default(),
    };
    Ok(Conn::new(build_opts().ssl_opts(Some(ssl_opts))).await?)
}

#[cfg(not(feature = "tls"))]
async fn connect_require<F>(_build_opts: F, _require_mode: RequireMode) -> Result<Conn>
where
    F: Fn() -> OptsBuilder,
{
    anyhow::bail!(
        "TLS was requested (SslMode::Require*) but this build has no TLS backend. \
         Rebuild the MySQL plugin with the `tls` feature enabled, or use \
         SslMode::Disabled / SslMode::IfAvailable."
    )
}

#[cfg(feature = "tls")]
fn relaxed_ssl_opts() -> mysql_async::SslOpts {
    mysql_async::SslOpts::default()
        .with_danger_accept_invalid_certs(true)
        .with_danger_skip_domain_validation(true)
}

#[cfg(feature = "tls")]
fn verify_ca_ssl_opts() -> mysql_async::SslOpts {
    mysql_async::SslOpts::default().with_danger_skip_domain_validation(true)
}

#[cfg(all(test, not(feature = "tls")))]
mod tests {
    use super::*;

    fn build_opts() -> OptsBuilder {
        OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(3306)
    }

    /// Without the `tls` feature, a TLS-requiring mode must return an error
    /// (from the guard) rather than attempting a connection or panicking.
    #[tokio::test]
    async fn require_without_tls_backend_errors() {
        for mode in [
            SslMode::Require,
            SslMode::RequireVerifyCa,
            SslMode::RequireVerifyFull,
        ] {
            let result = connect_with_ssl_mode(build_opts, mode).await;
            assert!(
                result.is_err(),
                "expected {mode:?} to error without a TLS backend"
            );
            let message = result.unwrap_err().to_string();
            assert!(
                message.contains("TLS"),
                "error message should mention TLS, got: {message}"
            );
        }
    }
}
