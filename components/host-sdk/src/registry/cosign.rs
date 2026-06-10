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
//! verifying the certificate chain back to the Sigstore root CA, and
//! verifying the ECDSA-P256 signature over the payload.
//!
//! Supports two cosign storage formats:
//! - **Simplesigning** (legacy): `sha256-DIGEST.sig` tag
//! - **Sigstore Bundle** (v0.3+): `sha256-DIGEST` referrers tag
//!
//! This replaces the `sigstore-rs` library with a lightweight, focused
//! implementation using `oci-client`, `x509-parser`, `p256`, and `p384`.

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use p256::ecdsa::{signature::Verifier as _, DerSignature, VerifyingKey};

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

/// Status of a cosign signature check.
#[derive(Debug, Clone)]
pub enum SignatureStatus {
    /// No signature found — the artifact is unsigned.
    Unsigned,
    /// Signature is cryptographically valid and the certificate chains to the Sigstore root CA.
    Verified(VerificationResult),
    /// A signature exists but verification failed — the artifact may have been tampered with.
    Tampered(String),
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
    /// Returns the full `SignatureStatus`: `Unsigned`, `Verified`, or `Tampered`.
    pub async fn verify_plugin(
        &self,
        oci_reference: &str,
        auth: &oci_client::secrets::RegistryAuth,
    ) -> SignatureStatus {
        if !self.config.enabled {
            return SignatureStatus::Unsigned;
        }

        info!("Verifying cosign signature for {oci_reference}...");
        verify_cosign_signature(oci_reference, auth).await
    }

    /// Verify multiple plugins in parallel against the OCI registry.
    ///
    /// Each entry is `(oci_reference, filename)`.
    /// Returns `(filename, SignatureStatus)` for each plugin.
    pub async fn verify_batch(
        &self,
        plugins: Vec<(String, String)>,
        auth: &oci_client::secrets::RegistryAuth,
    ) -> Vec<(String, SignatureStatus)> {
        if !self.config.enabled || plugins.is_empty() {
            return plugins
                .into_iter()
                .map(|(_, f)| (f, SignatureStatus::Unsigned))
                .collect();
        }

        info!(
            "Verifying {} plugin signature(s) against registry...",
            plugins.len()
        );

        let mut join_set = tokio::task::JoinSet::new();
        for (reference, filename) in plugins {
            let auth = auth.clone();
            join_set.spawn(async move {
                let status = verify_cosign_signature(&reference, &auth).await;
                match &status {
                    SignatureStatus::Verified(vr) => {
                        info!(
                            "✓ {reference} — signed by (issuer={}, subject={})",
                            vr.issuer, vr.subject
                        );
                    }
                    SignatureStatus::Unsigned => {
                        debug!("⊘ {reference} — unsigned");
                    }
                    SignatureStatus::Tampered(reason) => {
                        warn!("⚠ {reference} — TAMPERED: {reason}");
                    }
                }
                (filename, status)
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

// ── Embedded Sigstore CA certificates ──────────────────────────────────
//
// These are the public Sigstore trust-root certificates used by Fulcio.
// They are long-lived and rarely rotate (root valid until 2031).
// Source: https://fulcio.sigstore.dev/api/v2/trustBundle

/// Sigstore root CA (`O=sigstore.dev, CN=sigstore`), P-384, self-signed.
/// Retained for reference; only the intermediate is used for chain verification.
#[allow(dead_code)]
const SIGSTORE_ROOT_CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIB9zCCAXygAwIBAgIUALZNAPFdxHPwjeDloDwyYChAO/4wCgYIKoZIzj0EAwMw
KjEVMBMGA1UEChMMc2lnc3RvcmUuZGV2MREwDwYDVQQDEwhzaWdzdG9yZTAeFw0y
MTEwMDcxMzU2NTlaFw0zMTEwMDUxMzU2NThaMCoxFTATBgNVBAoTDHNpZ3N0b3Jl
LmRldjERMA8GA1UEAxMIc2lnc3RvcmUwdjAQBgcqhkjOPQIBBgUrgQQAIgNiAAT7
XeFT4rb3PQGwS4IajtLk3/OlnpgangaBclYpsYBr5i+4ynB07ceb3LP0OIOZdxex
X69c5iVuyJRQ+Hz05yi+UF3uBWAlHpiS5sh0+H2GHE7SXrk1EC5m1Tr19L9gg92j
YzBhMA4GA1UdDwEB/wQEAwIBBjAPBgNVHRMBAf8EBTADAQH/MB0GA1UdDgQWBBRY
wB5fkUWlZql6zJChkyLQKsXF+jAfBgNVHSMEGDAWgBRYwB5fkUWlZql6zJChkyLQ
KsXF+jAKBggqhkjOPQQDAwNpADBmAjEAj1nHeXZp+13NWBNa+EDsDP8G1WWg1tCM
WP/WHPqpaVo0jhsweNFZgSs0eE7wYI4qAjEA2WB9ot98sIkoF3vZYdd3/VtWB5b9
TNMea7Ix/stJ5TfcLLeABLE4BNJOsQ4vnBHJ
-----END CERTIFICATE-----";

/// Sigstore intermediate CA (`O=sigstore.dev, CN=sigstore-intermediate`), P-384.
const SIGSTORE_INTERMEDIATE_CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIICGjCCAaGgAwIBAgIUALnViVfnU0brJasmRkHrn/UnfaQwCgYIKoZIzj0EAwMw
KjEVMBMGA1UEChMMc2lnc3RvcmUuZGV2MREwDwYDVQQDEwhzaWdzdG9yZTAeFw0y
MjA0MTMyMDA2MTVaFw0zMTEwMDUxMzU2NThaMDcxFTATBgNVBAoTDHNpZ3N0b3Jl
LmRldjEeMBwGA1UEAxMVc2lnc3RvcmUtaW50ZXJtZWRpYXRlMHYwEAYHKoZIzj0C
AQYFK4EEACIDYgAE8RVS/ysH+NOvuDZyPIZtilgUF9NlarYpAd9HP1vBBH1U5CV7
7LSS7s0ZiH4nE7Hv7ptS6LvvR/STk798LVgMzLlJ4HeIfF3tHSaexLcYpSASr1kS
0N/RgBJz/9jWCiXno3sweTAOBgNVHQ8BAf8EBAMCAQYwEwYDVR0lBAwwCgYIKwYB
BQUHAwkwEgYDVR0TAQH/BAgwBgEB/wIBADAdBgNVHQ4EFgQU39Ppz1YkEZb5qNjp
KFWixi4YZD8wHwYDVR0jBBgwFoAUWMAeX5FFpWapesyQoZMi0CrFxfowCgYIKoZI
zj0EAwMDZwAwZAIwPCsQK4DYiZYDPIaDi5HFKnfxXx6ASSVmERfsynYBiX2X6SJR
nZU84/9DZdnFvvxmAjBOt6QpBlc4J/0DxvkTCqpclvziL6BCCPnjdlIB3Pu3BxsP
mygUY7Ii2zbdCdliiow=
-----END CERTIFICATE-----";

/// Verify a cosign signature for an OCI reference.
///
/// Returns `SignatureStatus::Unsigned` if no signature tag is found,
/// `SignatureStatus::Verified` if valid, or `SignatureStatus::Tampered` if
/// a signature exists but ECDSA or certificate chain verification fails.
async fn verify_cosign_signature(
    oci_reference: &str,
    auth: &oci_client::secrets::RegistryAuth,
) -> SignatureStatus {
    match verify_cosign_inner(oci_reference, auth).await {
        Ok(status) => status,
        Err(e) => {
            warn!("Signature verification error for {oci_reference}: {e}");
            SignatureStatus::Unsigned
        }
    }
}

/// Inner implementation that returns Result for ergonomic error handling.
async fn verify_cosign_inner(
    oci_reference: &str,
    auth: &oci_client::secrets::RegistryAuth,
) -> Result<SignatureStatus> {
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

    if let Ok((oci_client::manifest::OciManifest::ImageIndex(idx), _)) =
        client.pull_manifest(&referrers_ref, auth).await.as_ref()
    {
        if !idx.manifests.is_empty() {
            debug!("Found referrers index with {} entries", idx.manifests.len());
            let status = try_verify_bundle(&client, &repo, idx, auth, oci_reference).await;
            if !matches!(status, SignatureStatus::Unsigned) {
                return Ok(status);
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
            return Ok(SignatureStatus::Unsigned);
        }
    };

    Ok(try_verify_simplesigning(&client, &sig_ref, &sig_manifest, auth, oci_reference).await)
}

/// Verify a sigstore bundle (v0.3+) from a referrers image index.
///
/// The index contains inner manifests whose single layer is a JSON sigstore bundle
/// with a DSSE envelope. The signature is ECDSA-P256 over the DSSE PAE encoding.
/// After ECDSA verification, the Fulcio leaf certificate is validated against the
/// embedded Sigstore intermediate CA.
async fn try_verify_bundle(
    client: &oci_client::Client,
    repo: &str,
    idx: &oci_client::manifest::OciImageIndex,
    auth: &oci_client::secrets::RegistryAuth,
    oci_reference: &str,
) -> SignatureStatus {
    for entry in &idx.manifests {
        let inner_ref: oci_client::Reference = match format!("{repo}@{}", entry.digest).parse() {
            Ok(r) => r,
            Err(_) => continue,
        };

        let (inner_manifest, _) = match client.pull_manifest(&inner_ref, auth).await {
            Ok(m) => m,
            Err(_) => continue,
        };

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
        if client
            .pull_blob(&inner_ref, layer_desc, &mut buf)
            .await
            .is_err()
        {
            return SignatureStatus::Tampered("failed to pull sigstore bundle blob".into());
        }

        let bundle: serde_json::Value = match serde_json::from_slice(&buf) {
            Ok(v) => v,
            Err(_) => {
                return SignatureStatus::Tampered("invalid sigstore bundle JSON".into());
            }
        };

        let envelope = match bundle.get("dsseEnvelope") {
            Some(e) => e,
            None => {
                return SignatureStatus::Tampered("no dsseEnvelope in bundle".into());
            }
        };

        let payload_b64 = match envelope.get("payload").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => {
                return SignatureStatus::Tampered("no payload in DSSE envelope".into());
            }
        };
        let payload_bytes = match base64_decode(payload_b64) {
            Ok(b) => b,
            Err(_) => {
                return SignatureStatus::Tampered("invalid base64 payload".into());
            }
        };

        let sig_b64 = match envelope
            .get("signatures")
            .and_then(|s| s.as_array())
            .and_then(|a| a.first())
            .and_then(|s| s.get("sig"))
            .and_then(|v| v.as_str())
        {
            Some(s) => s,
            None => {
                return SignatureStatus::Tampered("no signature in DSSE envelope".into());
            }
        };
        let sig_bytes = match base64_decode(sig_b64) {
            Ok(b) => b,
            Err(_) => {
                return SignatureStatus::Tampered("invalid base64 signature".into());
            }
        };

        let cert_b64 = match bundle
            .get("verificationMaterial")
            .and_then(|vm| vm.get("certificate"))
            .and_then(|c| c.get("rawBytes"))
            .and_then(|v| v.as_str())
        {
            Some(c) => c,
            None => {
                return SignatureStatus::Tampered("no certificate in verification material".into());
            }
        };
        let cert_der = match base64_decode(cert_b64) {
            Ok(b) => b,
            Err(_) => {
                return SignatureStatus::Tampered("invalid base64 certificate".into());
            }
        };

        let (_, cert) = match x509_parser::parse_x509_certificate(&cert_der) {
            Ok(c) => c,
            Err(e) => {
                return SignatureStatus::Tampered(format!("failed to parse certificate: {e}"));
            }
        };

        // Verify ECDSA-P256 signature over DSSE PAE
        let pub_key = cert
            .tbs_certificate
            .subject_pki
            .subject_public_key
            .data
            .to_vec();
        let verifying_key = match VerifyingKey::from_sec1_bytes(&pub_key) {
            Ok(k) => k,
            Err(e) => {
                return SignatureStatus::Tampered(format!("invalid ECDSA-P256 public key: {e}"));
            }
        };

        let payload_type = envelope
            .get("payloadType")
            .and_then(|v| v.as_str())
            .unwrap_or("application/vnd.in-toto+json");
        let pae = dsse_pae(payload_type, &payload_bytes);

        let signature = match DerSignature::from_bytes(&sig_bytes) {
            Ok(s) => s,
            Err(e) => {
                return SignatureStatus::Tampered(format!("invalid DER signature: {e}"));
            }
        };

        if let Err(e) = verifying_key.verify(&pae, &signature) {
            return SignatureStatus::Tampered(format!("ECDSA signature verification failed: {e}"));
        }

        debug!("ECDSA-P256 signature verified (sigstore bundle)");

        // Verify certificate chain: leaf → Sigstore intermediate CA
        if let Err(reason) = verify_fulcio_chain(&cert) {
            return SignatureStatus::Tampered(reason);
        }

        match extract_identity(&cert, oci_reference) {
            Ok(result) => return SignatureStatus::Verified(result),
            Err(e) => {
                return SignatureStatus::Tampered(format!("failed to extract identity: {e}"));
            }
        }
    }

    SignatureStatus::Unsigned
}

/// Verify a simplesigning-format cosign signature.
///
/// The signature manifest is an image with layers containing the simplesigning
/// JSON payload, with certificate and signature in layer annotations.
/// After ECDSA verification, the Fulcio leaf certificate is validated against the
/// embedded Sigstore intermediate CA.
async fn try_verify_simplesigning(
    client: &oci_client::Client,
    sig_ref: &oci_client::Reference,
    sig_manifest: &oci_client::manifest::OciManifest,
    auth: &oci_client::secrets::RegistryAuth,
    oci_reference: &str,
) -> SignatureStatus {
    let layer_count = match sig_manifest {
        oci_client::manifest::OciManifest::Image(img) => img.layers.len(),
        _ => {
            return SignatureStatus::Tampered("expected image manifest for cosign signature".into())
        }
    };

    if layer_count == 0 {
        return SignatureStatus::Tampered("cosign signature manifest has no layers".into());
    }

    let image_data = match client
        .pull(
            sig_ref,
            auth,
            vec![
                "application/vnd.dev.cosign.simplesigning.v1+json",
                "application/octet-stream",
            ],
        )
        .await
    {
        Ok(d) => d,
        Err(e) => {
            return SignatureStatus::Tampered(format!(
                "failed to pull cosign signature layers: {e}"
            ));
        }
    };

    for layer in &image_data.layers {
        let anns = match &layer.annotations {
            Some(a) => a,
            None => continue,
        };

        let cert_pem_str = match anns
            .get("dev.sigstore.cosign/certificate")
            .or_else(|| anns.get("dev.cosignproject.cosign/certificate"))
        {
            Some(c) => c,
            None => continue,
        };

        let sig_b64 = match anns.get("dev.cosignproject.cosign/signature") {
            Some(s) => s,
            None => {
                return SignatureStatus::Tampered("no signature annotation".into());
            }
        };

        let cert_pem = match pem::parse(cert_pem_str) {
            Ok(p) => p,
            Err(e) => {
                return SignatureStatus::Tampered(format!("failed to parse certificate PEM: {e}"));
            }
        };
        let (_, cert) = match x509_parser::parse_x509_certificate(cert_pem.contents()) {
            Ok(c) => c,
            Err(e) => {
                return SignatureStatus::Tampered(format!("failed to parse certificate: {e}"));
            }
        };

        let pub_key = cert
            .tbs_certificate
            .subject_pki
            .subject_public_key
            .data
            .to_vec();
        let verifying_key = match VerifyingKey::from_sec1_bytes(&pub_key) {
            Ok(k) => k,
            Err(e) => {
                return SignatureStatus::Tampered(format!("invalid ECDSA-P256 public key: {e}"));
            }
        };

        let sig_bytes = match base64_decode(sig_b64) {
            Ok(b) => b,
            Err(_) => {
                return SignatureStatus::Tampered("invalid base64 signature".into());
            }
        };
        let signature = match DerSignature::from_bytes(&sig_bytes) {
            Ok(s) => s,
            Err(e) => {
                return SignatureStatus::Tampered(format!("invalid DER signature: {e}"));
            }
        };

        if let Err(e) = verifying_key.verify(&layer.data, &signature) {
            return SignatureStatus::Tampered(format!("ECDSA signature verification failed: {e}"));
        }

        debug!("ECDSA-P256 signature verified (simplesigning)");

        // Verify certificate chain: leaf → Sigstore intermediate CA
        if let Err(reason) = verify_fulcio_chain(&cert) {
            return SignatureStatus::Tampered(reason);
        }

        match extract_identity(&cert, oci_reference) {
            Ok(result) => return SignatureStatus::Verified(result),
            Err(e) => {
                return SignatureStatus::Tampered(format!("failed to extract identity: {e}"));
            }
        }
    }

    SignatureStatus::Unsigned
}

/// Verify that a Fulcio leaf certificate was issued by the Sigstore intermediate CA.
///
/// The intermediate CA signs leaf certificates using ECDSA-SHA384 (P-384).
/// We verify the leaf cert's `signature_value` over its `tbs_certificate` DER encoding
/// using the intermediate CA's P-384 public key.
fn verify_fulcio_chain(leaf: &x509_parser::certificate::X509Certificate<'_>) -> Result<(), String> {
    // Parse the embedded intermediate CA certificate
    let intermediate_pem = pem::parse(SIGSTORE_INTERMEDIATE_CA_PEM)
        .map_err(|e| format!("failed to parse embedded intermediate CA PEM: {e}"))?;
    let (_, intermediate) = x509_parser::parse_x509_certificate(intermediate_pem.contents())
        .map_err(|e| format!("failed to parse embedded intermediate CA cert: {e}"))?;

    // Extract the intermediate's P-384 public key
    let intermediate_pub_key = intermediate
        .tbs_certificate
        .subject_pki
        .subject_public_key
        .data
        .to_vec();
    let p384_verifying_key = p384::ecdsa::VerifyingKey::from_sec1_bytes(&intermediate_pub_key)
        .map_err(|e| format!("invalid intermediate CA P-384 public key: {e}"))?;

    // The leaf's signature is over its TBS (To-Be-Signed) certificate DER encoding
    let tbs_der = leaf.tbs_certificate.as_ref();
    let leaf_signature = p384::ecdsa::DerSignature::from_bytes(&leaf.signature_value.data)
        .map_err(|e| format!("invalid leaf certificate signature: {e}"))?;

    use p384::ecdsa::signature::Verifier as _;
    p384_verifying_key
        .verify(tbs_der, &leaf_signature)
        .map_err(|e| {
            format!(
                "certificate chain verification failed — leaf cert not issued by Sigstore CA: {e}"
            )
        })?;

    debug!("Certificate chain verified: leaf → Sigstore intermediate CA");
    Ok(())
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
