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

//! Cosign signature verification for OCI plugin artifacts.
//!
//! Provides optional keyless signature verification using the Sigstore
//! transparency log. When enabled, downloaded plugins must have a valid
//! cosign signature from a trusted identity (OIDC issuer + subject pattern).

use anyhow::{bail, Context, Result};
use log::{debug, info};
use sigstore::cosign::CosignCapabilities;

/// A trusted signing identity for cosign verification.
#[derive(Debug, Clone)]
pub struct TrustedIdentity {
    /// OIDC issuer URL (must match exactly).
    pub issuer: String,
    /// Glob pattern to match against the certificate subject/SAN.
    /// Supports `*` wildcards (e.g., "https://github.com/drasi-project/*").
    pub subject_pattern: String,
}

/// Result of a successful signature verification.
#[derive(Debug, Clone)]
pub struct VerificationResult {
    /// The issuer that matched.
    pub issuer: String,
    /// The subject from the signing certificate.
    pub subject: String,
}

/// Default trusted identity: drasi-project GitHub Actions.
fn default_trusted_identities() -> Vec<TrustedIdentity> {
    vec![TrustedIdentity {
        issuer: "https://token.actions.githubusercontent.com".to_string(),
        subject_pattern: "https://github.com/drasi-project/*".to_string(),
    }]
}

/// Configuration for cosign verification.
#[derive(Debug, Clone)]
pub struct VerificationConfig {
    /// Whether verification is enabled.
    pub enabled: bool,
    /// Trusted identities. If empty when enabled, uses the default drasi-project identity.
    pub trusted_identities: Vec<TrustedIdentity>,
}

impl Default for VerificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trusted_identities: Vec::new(),
        }
    }
}

impl VerificationConfig {
    /// Get the effective trusted identities (uses defaults if none configured).
    pub fn effective_identities(&self) -> Vec<TrustedIdentity> {
        if self.trusted_identities.is_empty() {
            default_trusted_identities()
        } else {
            self.trusted_identities.clone()
        }
    }
}

/// Cosign signature verifier for OCI artifacts.
pub struct CosignVerifier {
    config: VerificationConfig,
}

impl CosignVerifier {
    /// Create a new verifier with the given configuration.
    pub fn new(config: VerificationConfig) -> Self {
        Self { config }
    }

    /// Whether verification is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Verify a plugin's cosign signature.
    ///
    /// If verification is disabled, returns `Ok(None)` immediately.
    /// If enabled, fetches the cosign signature from the registry and checks
    /// it against the configured trusted identities using the Sigstore
    /// transparency log (Rekor) and Fulcio certificate chain.
    ///
    /// Returns `Ok(Some(result))` with the matched identity on success,
    /// or an error if verification fails.
    pub async fn verify_plugin(
        &self,
        oci_reference: &str,
        auth: &oci_client::secrets::RegistryAuth,
    ) -> Result<Option<VerificationResult>> {
        if !self.config.enabled {
            return Ok(None);
        }

        info!("Verifying cosign signature for {oci_reference}...");

        // Initialize the Sigstore trust root (Fulcio CA + Rekor keys)
        let trust_root = sigstore::trust::sigstore::SigstoreTrustRoot::new(None)
            .await
            .context("failed to fetch Sigstore trust root")?;

        // Build cosign client with the trust root
        let mut cosign_client = sigstore::cosign::ClientBuilder::default()
            .with_trust_repository(&trust_root)
            .context("failed to configure trust repository")?
            .build()
            .context("failed to build cosign client")?;

        // Parse the OCI reference
        let sigstore_ref: sigstore::registry::OciReference = oci_reference
            .parse()
            .context("invalid OCI reference for sigstore")?;

        // Triangulate: find the cosign signature tag
        let (cosign_image, source_digest) = cosign_client
            .triangulate(&sigstore_ref, &convert_auth(auth))
            .await
            .context("failed to locate cosign signature")?;

        debug!("Cosign signature image: {cosign_image}, source digest: {source_digest}");

        // Fetch trusted signature layers
        let signature_layers = cosign_client
            .trusted_signature_layers(&convert_auth(auth), &source_digest, &cosign_image)
            .await
            .context("failed to fetch signature layers")?;

        if signature_layers.is_empty() {
            bail!(
                "no valid cosign signatures found for {oci_reference}. \
                 The plugin may not be signed or the signature may be invalid."
            );
        }

        // Check signature layers against trusted identities
        let identities = self.config.effective_identities();

        for layer in &signature_layers {
            if let Some(result) = check_layer_against_identities(layer, &identities) {
                info!(
                    "✓ {oci_reference} — signature verified (issuer={}, subject={})",
                    result.issuer, result.subject
                );
                return Ok(Some(result));
            }
        }

        // No matching identity found
        let identity_desc: Vec<String> = identities
            .iter()
            .map(|id| format!("issuer={}, subject={}", id.issuer, id.subject_pattern))
            .collect();
        bail!(
            "cosign signature found for {oci_reference} but no trusted identity matched. \
             Trusted identities: [{}]",
            identity_desc.join("; ")
        );
    }
}

/// Check a signature layer against a list of trusted identities.
fn check_layer_against_identities(
    layer: &sigstore::cosign::SignatureLayer,
    identities: &[TrustedIdentity],
) -> Option<VerificationResult> {
    // Extract issuer and subject from the certificate signature
    let cert_sig = layer.certificate_signature.as_ref()?;

    let issuer_str = cert_sig.issuer.as_deref().unwrap_or("");
    let subject_str = match &cert_sig.subject {
        sigstore::cosign::signature_layers::CertificateSubject::Email(email) => email.as_str(),
        sigstore::cosign::signature_layers::CertificateSubject::Uri(uri) => uri.as_str(),
    };

    debug!("Checking signature layer: issuer={issuer_str}, subject={subject_str}");

    for identity in identities {
        if identity.issuer == issuer_str && glob_match(&identity.subject_pattern, subject_str) {
            return Some(VerificationResult {
                issuer: issuer_str.to_string(),
                subject: subject_str.to_string(),
            });
        }
    }

    None
}

/// Simple glob matching supporting `*` wildcards.
///
/// Only supports `*` at the end or middle of the pattern (not recursive `**`).
fn glob_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    if !pattern.contains('*') {
        return pattern == value;
    }

    let parts: Vec<&str> = pattern.split('*').collect();

    if parts.len() == 2 {
        // Single wildcard: "prefix*suffix"
        let prefix = parts[0];
        let suffix = parts[1];
        return value.starts_with(prefix) && value.ends_with(suffix);
    }

    // Multiple wildcards: check each segment in order
    let mut remaining = value;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            if !remaining.ends_with(part) {
                return false;
            }
            return true;
        } else if let Some(pos) = remaining.find(part) {
            remaining = &remaining[pos + part.len()..];
        } else {
            return false;
        }
    }

    true
}

/// Convert our oci_client auth to sigstore auth.
fn convert_auth(auth: &oci_client::secrets::RegistryAuth) -> sigstore::registry::Auth {
    match auth {
        oci_client::secrets::RegistryAuth::Anonymous => sigstore::registry::Auth::Anonymous,
        oci_client::secrets::RegistryAuth::Basic(user, pass) => {
            sigstore::registry::Auth::Basic(user.clone(), pass.clone())
        }
        oci_client::secrets::RegistryAuth::Bearer(token) => {
            sigstore::registry::Auth::Bearer(token.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_match_exact() {
        assert!(glob_match("hello", "hello"));
        assert!(!glob_match("hello", "world"));
    }

    #[test]
    fn test_glob_match_wildcard_suffix() {
        assert!(glob_match(
            "https://github.com/drasi-project/*",
            "https://github.com/drasi-project/anything"
        ));
        assert!(glob_match("https://github.com/drasi-project/*", "https://github.com/drasi-project/source/postgres/.github/workflows/release.yml@refs/heads/main"));
        assert!(!glob_match(
            "https://github.com/drasi-project/*",
            "https://github.com/other-org/something"
        ));
    }

    #[test]
    fn test_glob_match_wildcard_all() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn test_glob_match_prefix_and_suffix() {
        assert!(glob_match("pre*suf", "pre-middle-suf"));
        assert!(!glob_match("pre*suf", "pre-middle-other"));
    }

    #[test]
    fn test_default_trusted_identities() {
        let defaults = default_trusted_identities();
        assert_eq!(defaults.len(), 1);
        assert_eq!(
            defaults[0].issuer,
            "https://token.actions.githubusercontent.com"
        );
        assert!(defaults[0].subject_pattern.contains("drasi-project"));
    }

    #[test]
    fn test_verification_config_effective_identities_uses_defaults() {
        let config = VerificationConfig {
            enabled: true,
            trusted_identities: Vec::new(),
        };
        let effective = config.effective_identities();
        assert_eq!(effective.len(), 1);
    }

    #[test]
    fn test_verification_config_effective_identities_uses_custom() {
        let config = VerificationConfig {
            enabled: true,
            trusted_identities: vec![TrustedIdentity {
                issuer: "https://custom-issuer.example.com".to_string(),
                subject_pattern: "https://github.com/my-org/*".to_string(),
            }],
        };
        let effective = config.effective_identities();
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].issuer, "https://custom-issuer.example.com");
    }
}
