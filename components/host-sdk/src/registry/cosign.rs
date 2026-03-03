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
//! Implements keyless signature verification by directly inspecting the
//! cosign signature tag in the OCI registry, parsing the Fulcio certificate,
//! and verifying the ECDSA-P256 signature over the simplesigning payload.
//!
//! This replaces the `sigstore-rs` library with a lightweight, focused
//! implementation that only needs `oci-client`, `x509-parser`, and `p256`.

use anyhow::{bail, Context, Result};
use log::{debug, info};
use p256::ecdsa::{signature::Verifier, DerSignature, VerifyingKey};

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
    /// The OIDC issuer from the Fulcio certificate.
    pub issuer: String,
    /// The subject (SAN URI or email) from the Fulcio certificate.
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
#[derive(Debug, Clone, Default)]
pub struct VerificationConfig {
    /// Whether verification is enabled.
    pub enabled: bool,
    /// Trusted identities. If empty when enabled, uses the default drasi-project identity.
    pub trusted_identities: Vec<TrustedIdentity>,
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
    /// Pulls the cosign signature tag from the registry, parses the Fulcio
    /// certificate, verifies the ECDSA-P256 signature, and extracts the
    /// signer identity (OIDC issuer + subject).
    ///
    /// Returns `Ok(Some(result))` with the signer identity on success,
    /// `Ok(None)` if the plugin is unsigned, or an error if verification fails.
    pub async fn verify_plugin(
        &self,
        oci_reference: &str,
        auth: &oci_client::secrets::RegistryAuth,
    ) -> Result<Option<VerificationResult>> {
        if !self.config.enabled {
            return Ok(None);
        }

        info!("Verifying cosign signature for {oci_reference}...");
        verify_cosign_signature(oci_reference, auth).await
    }

    /// Verify multiple plugins in parallel against the OCI registry.
    ///
    /// Each entry is `(oci_reference, filename)`.
    /// Returns `(filename, Option<VerificationResult>)` for each plugin.
    /// `None` means unsigned or verification failed.
    pub async fn verify_batch(
        &self,
        plugins: Vec<(String, String)>,
        auth: &oci_client::secrets::RegistryAuth,
    ) -> Vec<(String, Option<VerificationResult>)> {
        if !self.config.enabled || plugins.is_empty() {
            return plugins.into_iter().map(|(_, f)| (f, None)).collect();
        }

        info!(
            "Verifying {} plugin signature(s) against registry...",
            plugins.len()
        );

        let mut join_set = tokio::task::JoinSet::new();
        for (reference, filename) in plugins {
            let auth = auth.clone();
            join_set.spawn(async move {
                match verify_cosign_signature(&reference, &auth).await {
                    Ok(Some(vr)) => {
                        info!(
                            "✓ {} — signed by (issuer={}, subject={})",
                            reference, vr.issuer, vr.subject
                        );
                        (filename, Some(vr))
                    }
                    Ok(None) => {
                        debug!("⊘ {} — unsigned", reference);
                        (filename, None)
                    }
                    Err(e) => {
                        log::warn!("✗ {} — verification failed: {e}", reference);
                        (filename, None)
                    }
                }
            });
        }

        let mut results = Vec::new();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(r) => results.push(r),
                Err(e) => log::error!("Verification task panicked: {e}"),
            }
        }
        results
    }
}

/// Fulcio OIDC Issuer extension OID (v1): 1.3.6.1.4.1.57264.1.1
const OID_FULCIO_ISSUER_V1: &str = "1.3.6.1.4.1.57264.1.1";

/// Fulcio OIDC Issuer extension OID (v2): 1.3.6.1.4.1.57264.1.8
const OID_FULCIO_ISSUER_V2: &str = "1.3.6.1.4.1.57264.1.8";

/// Verify a cosign signature for an OCI reference.
///
/// Supports two cosign signature storage formats:
/// 1. **Simplesigning** (legacy): `sha256-DIGEST.sig` tag → image manifest with
///    signature + certificate in layer annotations.
/// 2. **Sigstore Bundle** (v0.3+): `sha256-DIGEST` referrers tag → image index →
///    inner manifest with a `application/vnd.dev.sigstore.bundle.v0.3+json` layer
///    containing a DSSE envelope + certificate.
async fn verify_cosign_signature(
    oci_reference: &str,
    auth: &oci_client::secrets::RegistryAuth,
) -> Result<Option<VerificationResult>> {
    let parsed: oci_client::Reference = oci_reference.parse().context("invalid OCI reference")?;

    let client = oci_client::Client::new(oci_client::client::ClientConfig {
        protocol: oci_client::client::ClientProtocol::Https,
        ..Default::default()
    });

    // Get the digest (either from the reference or by pulling the manifest)
    let digest = if let Some(d) = parsed.digest() {
        d.to_string()
    } else {
        let (_manifest, d) = client
            .pull_manifest(&parsed, auth)
            .await
            .context("failed to pull manifest for digest")?;
        d
    };

    let digest_hex = digest
        .strip_prefix("sha256:")
        .context("expected sha256 digest")?;

    let repo = format!("{}/{}", parsed.registry(), parsed.repository());

    // Try sigstore bundle format first (sha256-DIGEST referrers tag)
    let referrers_tag = format!("{repo}:sha256-{digest_hex}");
    debug!("Looking for sigstore bundle at: {referrers_tag}");

    let referrers_ref: oci_client::Reference = referrers_tag
        .parse()
        .context("failed to construct referrers reference")?;

    if let Ok((manifest, _)) = client.pull_manifest(&referrers_ref, auth).await {
        if let oci_client::manifest::OciManifest::ImageIndex(idx) = &manifest {
            if !idx.manifests.is_empty() {
                debug!("Found referrers index with {} entries", idx.manifests.len());
                if let Some(result) =
                    try_verify_bundle(&client, &repo, idx, auth, oci_reference).await?
                {
                    return Ok(Some(result));
                }
            }
        }
    }

    // Fall back to simplesigning format (sha256-DIGEST.sig tag)
    let sig_tag = format!("{repo}:sha256-{digest_hex}.sig");
    debug!("Looking for simplesigning signature at: {sig_tag}");

    let sig_ref: oci_client::Reference = sig_tag
        .parse()
        .context("failed to construct signature reference")?;

    let sig_manifest = match client.pull_manifest(&sig_ref, auth).await {
        Ok((m, _)) => m,
        Err(_) => {
            debug!("No cosign signature found for {oci_reference}");
            return Ok(None);
        }
    };

    try_verify_simplesigning(&client, &sig_ref, &sig_manifest, auth, oci_reference).await
}

/// Verify a sigstore bundle (v0.3+) from a referrers image index.
///
/// The index contains inner manifests whose single layer is a JSON sigstore bundle
/// with a DSSE envelope. The signature is ECDSA-P256 over the DSSE PAE encoding.
async fn try_verify_bundle(
    client: &oci_client::Client,
    repo: &str,
    idx: &oci_client::manifest::OciImageIndex,
    auth: &oci_client::secrets::RegistryAuth,
    oci_reference: &str,
) -> Result<Option<VerificationResult>> {
    for entry in &idx.manifests {
        let inner_ref: oci_client::Reference = format!("{repo}@{}", entry.digest).parse()?;

        let (inner_manifest, _) = client
            .pull_manifest(&inner_ref, auth)
            .await
            .context("failed to pull inner signature manifest")?;

        let img = match &inner_manifest {
            oci_client::manifest::OciManifest::Image(img) => img,
            _ => continue,
        };

        if img.layers.is_empty() {
            continue;
        }

        let layer_desc = &img.layers[0];

        // Only process sigstore bundle layers
        if !layer_desc.media_type.contains("sigstore.bundle") {
            continue;
        }

        let mut buf = Vec::new();
        client
            .pull_blob(&inner_ref, layer_desc, &mut buf)
            .await
            .context("failed to pull sigstore bundle blob")?;

        let bundle: serde_json::Value =
            serde_json::from_slice(&buf).context("failed to parse sigstore bundle JSON")?;

        let envelope = bundle
            .get("dsseEnvelope")
            .context("no dsseEnvelope in bundle")?;

        let payload_b64 = envelope
            .get("payload")
            .and_then(|v| v.as_str())
            .context("no payload in DSSE envelope")?;
        let payload_bytes = base64_decode(payload_b64)?;

        let sig_b64 = envelope
            .get("signatures")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.get("sig"))
            .and_then(|v| v.as_str())
            .context("no signature in DSSE envelope")?;
        let sig_bytes = base64_decode(sig_b64)?;

        let cert_b64 = bundle
            .get("verificationMaterial")
            .and_then(|vm| vm.get("certificate"))
            .and_then(|c| c.get("rawBytes"))
            .and_then(|v| v.as_str())
            .context("no certificate in verification material")?;
        let cert_der = base64_decode(cert_b64)?;

        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der)
            .map_err(|e| anyhow::anyhow!("failed to parse certificate: {e}"))?;

        let pub_key = cert
            .tbs_certificate
            .subject_pki
            .subject_public_key
            .data
            .to_vec();
        let verifying_key = VerifyingKey::from_sec1_bytes(&pub_key)
            .context("failed to parse ECDSA-P256 public key")?;

        // DSSE PAE: "DSSEv1 <len(type)> <type> <len(payload)> <payload>"
        let payload_type = envelope
            .get("payloadType")
            .and_then(|v| v.as_str())
            .unwrap_or("application/vnd.in-toto+json");
        let pae = dsse_pae(payload_type, &payload_bytes);

        let signature = DerSignature::from_bytes(&sig_bytes)
            .map_err(|e| anyhow::anyhow!("failed to parse DER signature: {e}"))?;

        verifying_key
            .verify(&pae, &signature)
            .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))?;

        debug!("ECDSA-P256 signature verified (sigstore bundle)");
        let result = extract_identity(&cert, oci_reference)?;
        return Ok(Some(result));
    }

    Ok(None)
}

/// Verify a simplesigning-format cosign signature.
///
/// The signature manifest is an image with layers containing the simplesigning
/// JSON payload, with certificate and signature in layer annotations.
async fn try_verify_simplesigning(
    client: &oci_client::Client,
    sig_ref: &oci_client::Reference,
    sig_manifest: &oci_client::manifest::OciManifest,
    auth: &oci_client::secrets::RegistryAuth,
    oci_reference: &str,
) -> Result<Option<VerificationResult>> {
    let layer_count = match sig_manifest {
        oci_client::manifest::OciManifest::Image(img) => img.layers.len(),
        _ => bail!("expected image manifest for cosign signature"),
    };

    if layer_count == 0 {
        bail!("cosign signature manifest has no layers");
    }

    let image_data = client
        .pull(
            sig_ref,
            auth,
            vec![
                "application/vnd.dev.cosign.simplesigning.v1+json",
                "application/octet-stream",
            ],
        )
        .await
        .context("failed to pull cosign signature layers")?;

    for layer in &image_data.layers {
        let anns = match &layer.annotations {
            Some(a) => a,
            None => continue,
        };

        let cert_pem_str = anns
            .get("dev.sigstore.cosign/certificate")
            .or_else(|| anns.get("dev.cosignproject.cosign/certificate"))
            .context("no certificate annotation")?;

        let sig_b64 = anns
            .get("dev.cosignproject.cosign/signature")
            .context("no signature annotation")?;

        let cert_pem = pem::parse(cert_pem_str).context("failed to parse certificate PEM")?;
        let (_, cert) = x509_parser::parse_x509_certificate(cert_pem.contents())
            .map_err(|e| anyhow::anyhow!("failed to parse certificate: {e}"))?;

        let pub_key = cert
            .tbs_certificate
            .subject_pki
            .subject_public_key
            .data
            .to_vec();
        let verifying_key = VerifyingKey::from_sec1_bytes(&pub_key)
            .context("failed to parse ECDSA-P256 public key")?;

        let sig_bytes = base64_decode(sig_b64)?;
        let signature = DerSignature::from_bytes(&sig_bytes)
            .map_err(|e| anyhow::anyhow!("failed to parse DER signature: {e}"))?;

        verifying_key
            .verify(&layer.data, &signature)
            .map_err(|e| anyhow::anyhow!("signature verification failed: {e}"))?;

        debug!("ECDSA-P256 signature verified (simplesigning)");
        let result = extract_identity(&cert, oci_reference)?;
        return Ok(Some(result));
    }

    bail!("no processable signature layers found")
}

/// Build the DSSE Pre-Authentication Encoding (PAE).
fn dsse_pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut pae = Vec::new();
    pae.extend_from_slice(b"DSSEv1 ");
    pae.extend_from_slice(payload_type.len().to_string().as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload_type.as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload.len().to_string().as_bytes());
    pae.push(b' ');
    pae.extend_from_slice(payload);
    pae
}

/// Extract OIDC issuer and subject from a Fulcio certificate.
fn extract_identity(
    cert: &x509_parser::certificate::X509Certificate<'_>,
    oci_reference: &str,
) -> Result<VerificationResult> {
    let mut issuer = String::new();
    let mut subject = String::new();

    for ext in cert.extensions() {
        let oid = ext.oid.to_string();

        if oid == OID_FULCIO_ISSUER_V2 {
            issuer = decode_der_utf8string(ext.value)
                .or_else(|| std::str::from_utf8(ext.value).ok().map(String::from))
                .unwrap_or_default();
        } else if oid == OID_FULCIO_ISSUER_V1 && issuer.is_empty() {
            issuer = std::str::from_utf8(ext.value).unwrap_or("").to_string();
        }

        if ext.oid == x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME {
            if let x509_parser::extensions::ParsedExtension::SubjectAlternativeName(san) =
                ext.parsed_extension()
            {
                for name in &san.general_names {
                    match name {
                        x509_parser::extensions::GeneralName::RFC822Name(email) => {
                            subject = email.to_string();
                        }
                        x509_parser::extensions::GeneralName::URI(uri) => {
                            subject = uri.to_string();
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    if issuer.is_empty() {
        bail!("no OIDC issuer found in Fulcio certificate extensions");
    }

    if subject.is_empty() {
        bail!("no subject found in certificate SAN");
    }

    info!("✓ {oci_reference} — signed by (issuer={issuer}, subject={subject})");
    Ok(VerificationResult { issuer, subject })
}

/// Decode a DER-encoded UTF8String (tag 0x0c).
fn decode_der_utf8string(data: &[u8]) -> Option<String> {
    if data.len() > 2 && data[0] == 0x0c {
        let len = data[1] as usize;
        if data.len() >= 2 + len {
            return std::str::from_utf8(&data[2..2 + len])
                .ok()
                .map(String::from);
        }
    }
    None
}

/// Decode base64 (standard alphabet).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.as_bytes())
        .context("base64 decode error")
}

/// Check whether a verification result matches any of the trusted identities.
pub fn matches_trusted_identity(
    result: &VerificationResult,
    identities: &[TrustedIdentity],
) -> bool {
    identities
        .iter()
        .any(|id| id.issuer == result.issuer && glob_match(&id.subject_pattern, &result.subject))
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
        let prefix = parts[0];
        let suffix = parts[1];
        return value.starts_with(prefix) && value.ends_with(suffix);
    }

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

    #[test]
    fn test_matches_trusted_identity() {
        let result = VerificationResult {
            issuer: "https://token.actions.githubusercontent.com".to_string(),
            subject: "https://github.com/drasi-project/source/postgres/.github/workflows/release.yml@refs/heads/main".to_string(),
        };
        let trusted = default_trusted_identities();
        assert!(matches_trusted_identity(&result, &trusted));
    }

    #[test]
    fn test_matches_trusted_identity_wrong_issuer() {
        let result = VerificationResult {
            issuer: "https://other-issuer.example.com".to_string(),
            subject: "https://github.com/drasi-project/something".to_string(),
        };
        let trusted = default_trusted_identities();
        assert!(!matches_trusted_identity(&result, &trusted));
    }

    #[test]
    fn test_decode_der_utf8string() {
        // DER UTF8String: tag=0x0c, length=5, value="hello"
        let data = [0x0c, 0x05, b'h', b'e', b'l', b'l', b'o'];
        assert_eq!(decode_der_utf8string(&data), Some("hello".to_string()));
    }

    #[test]
    fn test_decode_der_utf8string_not_utf8string() {
        let data = [0x04, 0x03, b'f', b'o', b'o']; // OCTET STRING, not UTF8String
        assert_eq!(decode_der_utf8string(&data), None);
    }
}
