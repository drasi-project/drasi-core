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

//! Shared configuration types for MySQL plugins.

/// SSL mode for MySQL connections.
///
/// Shared by the MySQL source and bootstrap plugins. The actual TLS negotiation
/// is performed by [`crate::connect::connect_with_ssl_mode`], which only attempts
/// a TLS handshake when the crate is built with the `tls` feature enabled.
///
/// # Certificate verification
///
/// The `tls` backend is rustls. [`SslMode::IfAvailable`] attempts opportunistic
/// TLS **without** certificate verification and falls back to plaintext on
/// failure, so it only protects against passive eavesdroppers. The `Require*`
/// variants enforce TLS and verify the server: [`SslMode::Require`] and
/// [`SslMode::RequireVerifyFull`] perform full certificate-chain **and**
/// hostname verification, while [`SslMode::RequireVerifyCa`] verifies the chain
/// but skips hostname validation. When built **without** the `tls` feature, no
/// TLS is ever negotiated: [`SslMode::IfAvailable`] connects in plaintext and
/// the `Require*` variants return an error.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SslMode {
    /// Disable SSL encryption — always connect in plaintext.
    Disabled,
    /// Try SSL but fall back to an unencrypted connection if it fails
    /// (opportunistic TLS, equivalent to MySQL's `--ssl-mode=PREFERRED`).
    ///
    /// With the `tls` feature, TLS is attempted **without** certificate or
    /// hostname verification (invalid certs accepted, hostname checks skipped),
    /// falling back to plaintext on error. Without `tls`, connects in plaintext.
    ///
    /// # Security
    ///
    /// This mode is **not** safe against an active network attacker. Because it
    /// silently downgrades to plaintext when the TLS handshake fails, an attacker
    /// who disrupts the handshake (TCP reset, forged TLS alert) can strip TLS and
    /// force a plaintext connection — exposing credentials and query data — while
    /// only a `warn`-level log records the fallback. Use [`SslMode::Require`] (or
    /// a verifying variant) in any environment where credentials must be
    /// protected against active attackers.
    #[default]
    IfAvailable,
    /// Require an encrypted connection **with full verification** of the server
    /// certificate chain and hostname (equivalent to [`SslMode::RequireVerifyFull`]).
    ///
    /// Protects against both passive eavesdropping and active man-in-the-middle
    /// attacks. If your server uses a self-signed certificate or one whose
    /// hostname does not match, use [`SslMode::RequireVerifyCa`] (verify chain,
    /// skip hostname) instead. Requires the `tls` feature; otherwise the
    /// connection returns an error.
    Require,
    /// Require SSL with CA (certificate chain) verification, but **skip** hostname
    /// validation.
    ///
    /// Requires the `tls` feature; otherwise the connection returns an error.
    RequireVerifyCa,
    /// Require SSL with full verification: CA (certificate chain) **and** hostname
    /// validation.
    ///
    /// Requires the `tls` feature; otherwise the connection returns an error.
    RequireVerifyFull,
}

/// Table key configuration for MySQL sources and bootstrappers.
///
/// Maps a table name to the columns that form its primary key,
/// used to generate deterministic element IDs.
#[derive(Debug, Clone, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// Validates that a string is a safe SQL identifier (alphanumeric + underscore only).
pub fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_identifiers() {
        assert!(is_valid_identifier("users"));
        assert!(is_valid_identifier("order_items"));
        assert!(is_valid_identifier("Table1"));
        assert!(is_valid_identifier("_private"));
    }

    #[test]
    fn test_invalid_identifiers() {
        assert!(!is_valid_identifier(""));
        assert!(!is_valid_identifier("my table"));
        assert!(!is_valid_identifier("table;DROP"));
        assert!(!is_valid_identifier("my-table"));
        assert!(!is_valid_identifier("table.name"));
    }
}
