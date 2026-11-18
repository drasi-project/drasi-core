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

//! OCI registry client for pulling and inspecting plugin artifacts.

use crate::loader::{ordinary_abi_compatible, COMPUTATION_ABI_FAMILY, ORDINARY_ABI_FAMILY};
use crate::registry::cosign::{CosignVerifier, SignatureStatus};
use crate::registry::types::{
    annotations, media_types, PluginMetadataJson, PluginReference, RegistryAuth, RegistryConfig,
};
use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageManifest;
use oci_client::Reference;
use std::path::{Path, PathBuf};

/// Result of downloading a plugin, including the binary path and optional verification info.
#[derive(Debug)]
pub struct DownloadResult {
    /// Path to the downloaded binary file.
    pub path: PathBuf,
    /// Cosign signature verification status.
    pub verification: SignatureStatus,
}

/// Well-known package name used as a plugin directory index.
/// Each tag in this package represents a registered plugin (e.g., "source.postgres").
const PLUGIN_DIRECTORY_PACKAGE: &str = "drasi-plugin-directory";

/// Binary marker prefix of each declared ABI family.
fn abi_marker_prefix(family: &str) -> Option<&'static [u8]> {
    match family {
        ORDINARY_ABI_FAMILY => Some(drasi_ffi_primitives::PLUGIN_ABI_VERSION_MARKER_PREFIX),
        COMPUTATION_ABI_FAMILY => Some(drasi_computation_plugin_abi::ABI_VERSION_MARKER_PREFIX),
        _ => None,
    }
}

/// Validate a downloaded artifact before it is installed: the manifest
/// annotations, the metadata layer and the binary's own ABI marker must declare
/// exactly the same ABI family and version, the binary must not carry another
/// family's marker, and the host must accept that ABI with the loader's rule.
fn validate_binary_abi(
    binary: &[u8],
    metadata: &PluginMetadataJson,
    manifest: Option<&OciImageManifest>,
) -> Result<()> {
    let manifest = manifest.context("plugin artifact is missing its downloaded manifest")?;
    let declared = |key: &str| {
        manifest
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(key))
            .with_context(|| format!("missing ABI manifest annotation ({key})"))
    };
    let manifest_family = declared(annotations::ABI_FAMILY)?;
    let manifest_abi = declared(annotations::ABI_VERSION)?;
    let missing_metadata =
        "missing ABI metadata; legacy artifacts must be republished with validated ABI metadata";
    let metadata_family = metadata.abi_family.as_deref().context(missing_metadata)?;
    let metadata_abi = metadata.abi_version.as_deref().context(missing_metadata)?;
    if manifest_family != metadata_family {
        bail!("ABI family mismatch: manifest={manifest_family}, metadata={metadata_family}");
    }
    let prefix = abi_marker_prefix(manifest_family)
        .with_context(|| format!("unsupported plugin ABI family {manifest_family}"))?;
    let binary_abi = drasi_ffi_primitives::read_abi_version_marker(binary, prefix)
        .map_err(|error| anyhow::anyhow!("{manifest_family} binary ABI marker: {error}"))?;
    for (family, other) in [
        (
            ORDINARY_ABI_FAMILY,
            drasi_ffi_primitives::PLUGIN_ABI_VERSION_MARKER_PREFIX,
        ),
        (
            COMPUTATION_ABI_FAMILY,
            drasi_computation_plugin_abi::ABI_VERSION_MARKER_PREFIX,
        ),
    ] {
        if family != manifest_family && binary.windows(other.len()).any(|window| window == other) {
            bail!("{manifest_family} artifact binary also declares the {family} ABI family");
        }
    }
    if manifest_abi != metadata_abi || metadata_abi != binary_abi {
        bail!(
            "ABI metadata mismatch: manifest={manifest_abi}, metadata={metadata_abi}, binary={binary_abi}"
        );
    }
    let (compatible, host_abi) = if manifest_family == ORDINARY_ABI_FAMILY {
        (
            ordinary_abi_compatible(binary_abi),
            drasi_plugin_sdk::ffi::metadata::FFI_SDK_VERSION,
        )
    } else {
        (
            drasi_computation_plugin_abi::accepts_abi_version(binary_abi),
            drasi_computation_plugin_abi::ABI_VERSION,
        )
    };
    if !compatible {
        bail!("{manifest_family} ABI version mismatch: host={host_abi}, binary={binary_abi}");
    }
    Ok(())
}

/// OCI registry client for interacting with plugin artifact registries.
pub struct OciRegistryClient {
    client: oci_client::Client,
    config: RegistryConfig,
    verifier: CosignVerifier,
}

impl OciRegistryClient {
    /// Create a new OCI registry client.
    pub fn new(config: RegistryConfig) -> Self {
        let client_config = ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        };
        Self {
            client: oci_client::Client::new(client_config),
            config,
            verifier: CosignVerifier::new(Default::default()),
        }
    }

    /// Create a new OCI registry client with cosign verification.
    pub fn with_verifier(config: RegistryConfig, verifier: CosignVerifier) -> Self {
        let client_config = ClientConfig {
            protocol: ClientProtocol::Https,
            ..Default::default()
        };
        Self {
            client: oci_client::Client::new(client_config),
            config,
            verifier,
        }
    }

    #[cfg(test)]
    pub(super) fn for_test_registry(config: RegistryConfig) -> Self {
        Self {
            client: oci_client::Client::new(ClientConfig {
                protocol: ClientProtocol::Http,
                ..Default::default()
            }),
            config,
            verifier: CosignVerifier::new(Default::default()),
        }
    }

    /// Get the OCI auth credentials.
    pub fn auth(&self) -> oci_client::secrets::RegistryAuth {
        match &self.config.auth {
            RegistryAuth::Anonymous => oci_client::secrets::RegistryAuth::Anonymous,
            RegistryAuth::Basic { username, password } => {
                oci_client::secrets::RegistryAuth::Basic(username.clone(), password.clone())
            }
        }
    }

    /// List available tags for a plugin reference.
    pub async fn list_tags(&self, reference: &str) -> Result<Vec<String>> {
        let parsed = PluginReference::parse(reference, &self.config.default_registry)?;
        let oci_ref: Reference = parsed
            .to_oci_reference()
            .parse()
            .context("invalid OCI reference")?;

        self.list_tags_all(&oci_ref).await
    }

    /// Fetch all tags across all pages from the registry.
    ///
    /// OCI registries may paginate tag listings (e.g., GHCR defaults to 100 tags per page).
    /// This method follows pagination using the `last` cursor until all tags are retrieved.
    async fn list_tags_all(&self, oci_ref: &Reference) -> Result<Vec<String>> {
        const PAGE_SIZE: usize = 1000;
        const MAX_TAGS: usize = 100_000;

        let mut all_tags = Vec::new();
        let mut last: Option<String> = None;

        loop {
            let response = match self
                .client
                .list_tags(oci_ref, &self.auth(), Some(PAGE_SIZE), last.as_deref())
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    // GHCR returns {"tags": null} instead of {"tags": []} on the
                    // page after the last real page. The oci-client crate
                    // deserializes `tags` as Vec<String> which rejects null.
                    // This is normal end-of-pagination behavior.
                    let err_str = format!("{e:#}");
                    if err_str.contains("invalid type: null") {
                        debug!("Registry returned null tags for {oci_ref}; end of pagination");
                        break;
                    }
                    return Err(e).context("failed to list tags");
                }
            };

            let page = response.tags;
            if page.is_empty() {
                break;
            }

            let new_last = page.last().cloned();
            if new_last == last {
                warn!(
                    "Registry pagination cursor did not advance for {oci_ref}; stopping to avoid infinite loop"
                );
                break;
            }
            last = new_last;
            all_tags.extend(page);

            if all_tags.len() >= MAX_TAGS {
                warn!(
                    "Tag listing for {oci_ref} reached safety cap of {MAX_TAGS} tags; results may be incomplete"
                );
                break;
            }
        }

        Ok(all_tags)
    }

    /// Fetch the manifest and annotations for a specific tag.
    pub async fn fetch_manifest_annotations(
        &self,
        reference: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        self.fetch_manifest_annotations_with_digest(reference)
            .await
            .map(|(annotations, _digest)| annotations)
    }

    /// Fetch a manifest's annotations together with the digest of that same
    /// manifest, so a resolved digest is the one whose annotations were checked.
    pub(crate) async fn fetch_manifest_annotations_with_digest(
        &self,
        reference: &str,
    ) -> Result<(std::collections::BTreeMap<String, String>, String)> {
        let oci_ref: Reference = reference.parse().context("invalid OCI reference")?;

        let (manifest, digest) = self
            .client
            .pull_manifest(&oci_ref, &self.auth())
            .await
            .context("failed to pull manifest")?;

        match manifest {
            oci_client::manifest::OciManifest::Image(img) => {
                Ok((img.annotations.unwrap_or_default(), digest))
            }
            oci_client::manifest::OciManifest::ImageIndex(_) => {
                bail!("expected image manifest, got image index; specify a platform-specific tag")
            }
        }
    }

    /// Fetch the plugin metadata JSON from the metadata layer.
    pub async fn fetch_metadata(&self, reference: &str) -> Result<PluginMetadataJson> {
        let oci_ref: Reference = reference.parse().context("invalid OCI reference")?;

        let image_data = self
            .client
            .pull(
                &oci_ref,
                &self.auth(),
                vec![
                    media_types::PLUGIN_METADATA,
                    media_types::PLUGIN_BINARY,
                    media_types::PLUGIN_CONFIG,
                ],
            )
            .await
            .context("failed to pull artifact")?;

        // Find the metadata layer
        for layer in &image_data.layers {
            if layer.media_type == media_types::PLUGIN_METADATA {
                let metadata: PluginMetadataJson = serde_json::from_slice(&layer.data)
                    .context("failed to parse plugin metadata JSON")?;
                return Ok(metadata);
            }
        }

        bail!("no metadata layer found in artifact")
    }

    /// Download a plugin binary to a destination directory.
    ///
    /// If cosign verification is enabled, the signature is verified BEFORE
    /// writing the binary to disk. Returns the path to the downloaded binary
    /// and the verification result (if verification was enabled and succeeded).
    pub async fn download_plugin(
        &self,
        reference: &str,
        dest_dir: &Path,
        filename: &str,
    ) -> Result<DownloadResult> {
        let oci_ref: Reference = reference.parse().context("invalid OCI reference")?;

        // Attempt cosign signature verification (best-effort).
        // Verification result is recorded but failures don't block download.
        let verification = self.verifier.verify_plugin(reference, &self.auth()).await;

        info!("Downloading plugin from {reference}...");

        let image_data = self
            .client
            .pull(
                &oci_ref,
                &self.auth(),
                vec![
                    media_types::PLUGIN_METADATA,
                    media_types::PLUGIN_BINARY,
                    media_types::PLUGIN_CONFIG,
                ],
            )
            .await
            .context("failed to pull artifact")?;

        // Find the binary layer
        for layer in &image_data.layers {
            if layer.media_type == media_types::PLUGIN_BINARY {
                let metadata_layer = image_data
                    .layers
                    .iter()
                    .find(|layer| layer.media_type == media_types::PLUGIN_METADATA)
                    .context("plugin artifact is missing its metadata layer")?;
                let metadata: PluginMetadataJson = serde_json::from_slice(&metadata_layer.data)
                    .context("failed to parse plugin metadata JSON")?;
                validate_binary_abi(&layer.data, &metadata, image_data.manifest.as_ref())
                    .with_context(|| format!("invalid plugin ABI metadata for {reference}"))?;

                let dest_path = dest_dir.join(filename);
                tokio::fs::create_dir_all(dest_dir)
                    .await
                    .context("failed to create destination directory")?;
                tokio::fs::write(&dest_path, &layer.data)
                    .await
                    .context("failed to write plugin binary")?;

                // Set executable permission on Unix
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let perms = std::fs::Permissions::from_mode(0o755);
                    std::fs::set_permissions(&dest_path, perms)
                        .context("failed to set executable permission")?;
                }

                let size_mb = layer.data.len() as f64 / 1_048_576.0;
                info!(
                    "Downloaded {} ({:.1} MB) → {}",
                    reference,
                    size_mb,
                    dest_path.display()
                );

                return Ok(DownloadResult {
                    path: dest_path,
                    verification,
                });
            }
        }

        bail!("no binary layer found in artifact")
    }

    /// Get the manifest digest for a reference.
    pub async fn get_digest(&self, reference: &str) -> Result<String> {
        let oci_ref: Reference = reference.parse().context("invalid OCI reference")?;

        let (_manifest, digest) = self
            .client
            .pull_manifest(&oci_ref, &self.auth())
            .await
            .context("failed to pull manifest")?;

        Ok(digest)
    }

    /// Get a reference to the cosign verifier.
    pub fn verifier(&self) -> &CosignVerifier {
        &self.verifier
    }

    /// Expand a short plugin reference to a full OCI reference.
    pub fn expand_reference(&self, reference: &str) -> Result<String> {
        let parsed = PluginReference::parse(reference, &self.config.default_registry)?;
        Ok(parsed.to_oci_reference())
    }

    /// Search for plugins matching a query.
    ///
    /// Discovers available plugins by scanning the `drasi-plugin-directory` package
    /// in the registry. Each tag in that package represents a plugin (e.g., "source.postgres").
    ///
    /// - If query contains `/` (e.g., "source/postgres"), searches for an exact match.
    /// - If query is a bare name (e.g., "postgres"), matches any plugin whose kind contains it.
    /// - If query is `*` or empty, lists all plugins.
    ///
    /// For each matched plugin, fetches available versions and groups by platform suffix.
    pub async fn search_plugins(&self, query: &str) -> Result<Vec<PluginSearchResult>> {
        use crate::registry::platform::strip_arch_suffix;

        let dir_ref = format!(
            "{}/{}",
            self.config.default_registry, PLUGIN_DIRECTORY_PACKAGE
        );

        // List all directory entries
        let dir_oci_ref: Reference = dir_ref.parse().context("invalid directory reference")?;
        let dir_tags = self
            .list_tags_all(&dir_oci_ref)
            .await
            .context("failed to list plugin directory — directory package may not exist yet")?;

        // Parse directory tags into (type, kind) pairs
        // Tags are formatted as "type.kind" (e.g., "source.postgres", "reaction.storedproc-mssql")
        let mut candidates: Vec<(String, String)> = Vec::new();
        for tag in &dir_tags {
            if let Some((ptype, kind)) = tag.split_once('.') {
                let plugin_ref = format!("{ptype}/{kind}");

                let matches = if query.is_empty() || query == "*" {
                    true
                } else if query.contains('/') {
                    // Exact type/kind match
                    plugin_ref == query
                } else {
                    // Bare name — match if kind contains the query
                    kind.contains(query)
                };

                if matches {
                    candidates.push((ptype.to_string(), kind.to_string()));
                }
            }
        }

        // For each matched plugin, fetch versions
        let mut results = Vec::new();
        for (ptype, kind) in &candidates {
            let plugin_ref = format!("{ptype}/{kind}");
            match self.list_tags(&plugin_ref).await {
                Ok(tags) => {
                    // Group tags by version, collecting platform suffixes
                    let mut version_map: std::collections::BTreeMap<String, Vec<String>> =
                        std::collections::BTreeMap::new();
                    for tag in &tags {
                        if let Some((version, suffix)) = strip_arch_suffix(tag) {
                            version_map
                                .entry(version.to_string())
                                .or_default()
                                .push(suffix.to_string());
                        }
                    }

                    let versions: Vec<PluginVersionInfo> = version_map
                        .into_iter()
                        .map(|(version, platforms)| PluginVersionInfo { version, platforms })
                        .collect();

                    results.push(PluginSearchResult {
                        reference: plugin_ref.clone(),
                        full_reference: self.expand_reference(&plugin_ref).unwrap_or_default(),
                        versions,
                    });
                }
                Err(_) => {
                    // Plugin exists in directory but package not accessible — skip
                }
            }
        }

        Ok(results)
    }
}

/// Result of a plugin search.
#[derive(Debug, Clone)]
pub struct PluginSearchResult {
    /// Short plugin reference (e.g., "source/postgres").
    pub reference: String,
    /// Full OCI reference (e.g., "ghcr.io/drasi-project/source/postgres").
    pub full_reference: String,
    /// Available versions with their platform suffixes.
    pub versions: Vec<PluginVersionInfo>,
}

/// A single version of a plugin with its available platforms.
#[derive(Debug, Clone)]
pub struct PluginVersionInfo {
    /// Version string (e.g., "0.1.8").
    pub version: String,
    /// Available platform suffixes (e.g., ["linux-amd64", "darwin-arm64"]).
    pub platforms: Vec<String>,
}

#[cfg(test)]
mod abi_tests {
    use super::*;

    const ORDINARY: &str = drasi_plugin_sdk::ffi::metadata::FFI_SDK_VERSION;
    const COMPUTATION: &str = drasi_computation_plugin_abi::ABI_VERSION;

    fn manifest(family: Option<&str>, abi: Option<&str>) -> OciImageManifest {
        let annotations: std::collections::BTreeMap<_, _> = [
            family.map(|family| (annotations::ABI_FAMILY.to_string(), family.to_string())),
            abi.map(|abi| (annotations::ABI_VERSION.to_string(), abi.to_string())),
        ]
        .into_iter()
        .flatten()
        .collect();
        OciImageManifest {
            annotations: Some(annotations),
            ..Default::default()
        }
    }

    fn metadata(family: Option<&str>, abi: Option<&str>) -> PluginMetadataJson {
        PluginMetadataJson {
            name: "drasi-source-mock".into(),
            kind: "mock".into(),
            plugin_type: "source".into(),
            version: "0.1.0".into(),
            sdk_version: "0.11.3".into(),
            abi_family: family.map(str::to_string),
            abi_version: abi.map(str::to_string),
            filename: None,
            core_version: "0.5.10".into(),
            lib_version: "0.9.3".into(),
            target_triple: "aarch64-apple-darwin".into(),
            description: None,
            license: None,
        }
    }

    fn ordinary_binary(abi: &str) -> String {
        format!("binary\0drasi-plugin-ffi-abi={abi}\0")
    }

    fn native_binary(abi: &str) -> String {
        format!("binary\0drasi-computation-plugin-abi={abi}\0")
    }

    fn validate(binary: &str, family: &str, metadata_abi: &str, manifest_abi: &str) -> Result<()> {
        validate_binary_abi(
            binary.as_bytes(),
            &metadata(Some(family), Some(metadata_abi)),
            Some(&manifest(Some(family), Some(manifest_abi))),
        )
    }

    #[test]
    fn accepts_matching_manifest_metadata_binary_and_host_abi_for_each_family() {
        validate(
            &ordinary_binary(ORDINARY),
            ORDINARY_ABI_FAMILY,
            ORDINARY,
            ORDINARY,
        )
        .expect("matching ordinary ABI");
        validate(
            &native_binary(COMPUTATION),
            COMPUTATION_ABI_FAMILY,
            COMPUTATION,
            COMPUTATION,
        )
        .expect("matching native ABI");
    }

    #[test]
    fn allows_host_patch_differences_but_requires_exact_artifact_agreement() {
        let mut patch = semver::Version::parse(ORDINARY).expect("host ABI");
        patch.patch += 1;
        let patch = patch.to_string();
        validate(
            &ordinary_binary(&patch),
            ORDINARY_ABI_FAMILY,
            &patch,
            &patch,
        )
        .expect("artifact agrees exactly, with a compatible host patch difference");

        for (manifest_abi, metadata_abi, binary_abi) in [
            (patch.as_str(), ORDINARY, ORDINARY),
            (ORDINARY, patch.as_str(), ORDINARY),
            (ORDINARY, ORDINARY, patch.as_str()),
        ] {
            let error = validate(
                &ordinary_binary(binary_abi),
                ORDINARY_ABI_FAMILY,
                metadata_abi,
                manifest_abi,
            )
            .expect_err("patch-compatible but inconsistent artifact metadata");
            let error = error.to_string();
            assert!(error.contains("ABI metadata mismatch"), "{error}");
            assert!(
                error.contains(&format!("manifest={manifest_abi}")),
                "{error}"
            );
            assert!(
                error.contains(&format!("metadata={metadata_abi}")),
                "{error}"
            );
            assert!(error.contains(&format!("binary={binary_abi}")), "{error}");
        }
    }

    #[test]
    fn rejects_missing_downloaded_manifest_or_abi_annotations() {
        let binary = ordinary_binary(ORDINARY);
        let metadata = metadata(Some(ORDINARY_ABI_FAMILY), Some(ORDINARY));
        let error = validate_binary_abi(binary.as_bytes(), &metadata, None)
            .expect_err("missing downloaded manifest");
        assert!(
            error
                .to_string()
                .contains("missing its downloaded manifest"),
            "{error}"
        );
        for (manifest, missing) in [
            (
                OciImageManifest {
                    annotations: None,
                    ..Default::default()
                },
                annotations::ABI_FAMILY,
            ),
            (manifest(None, Some(ORDINARY)), annotations::ABI_FAMILY),
            (
                manifest(Some(ORDINARY_ABI_FAMILY), None),
                annotations::ABI_VERSION,
            ),
        ] {
            let error = validate_binary_abi(binary.as_bytes(), &metadata, Some(&manifest))
                .expect_err("missing ABI annotation");
            assert!(error.to_string().contains(missing), "{error}");
        }
    }

    #[test]
    fn rejects_missing_or_incorrect_metadata_before_installation() {
        let binary = ordinary_binary(ORDINARY);
        let manifest = manifest(Some(ORDINARY_ABI_FAMILY), Some(ORDINARY));
        for metadata in [
            metadata(None, Some(ORDINARY)),
            metadata(Some(ORDINARY_ABI_FAMILY), None),
            metadata(Some(COMPUTATION_ABI_FAMILY), Some(ORDINARY)),
        ] {
            assert!(validate_binary_abi(binary.as_bytes(), &metadata, Some(&manifest)).is_err());
        }
        let error = validate(&binary, ORDINARY_ABI_FAMILY, "0.99.0", ORDINARY)
            .expect_err("incorrect metadata");
        assert!(
            error.to_string().contains("ABI metadata mismatch"),
            "{error}"
        );
        // No marker, or only the other family's marker, is never accepted.
        for binary in ["legacy binary".to_string(), native_binary(ORDINARY)] {
            assert!(validate(&binary, ORDINARY_ABI_FAMILY, ORDINARY, ORDINARY).is_err());
        }
        // A binary carrying both families' markers has an ambiguous ABI.
        let both = format!(
            "{}{}",
            ordinary_binary(ORDINARY),
            native_binary(COMPUTATION)
        );
        let error =
            validate(&both, ORDINARY_ABI_FAMILY, ORDINARY, ORDINARY).expect_err("ambiguous family");
        assert!(error.to_string().contains("also declares"), "{error}");
    }

    #[test]
    fn rejects_incompatible_binary_even_when_metadata_agrees() {
        let error = validate(
            &ordinary_binary("99.0.0"),
            ORDINARY_ABI_FAMILY,
            "99.0.0",
            "99.0.0",
        )
        .expect_err("incompatible binary");
        assert!(
            error.to_string().contains("ABI version mismatch"),
            "{error}"
        );
        let abi = semver::Version::parse(COMPUTATION).expect("native ABI");
        let newer = format!("{}.{}.0", abi.major, abi.minor + 1);
        let error = validate(
            &native_binary(&newer),
            COMPUTATION_ABI_FAMILY,
            &newer,
            &newer,
        )
        .expect_err("newer native minor");
        assert!(
            error
                .to_string()
                .contains(&format!("host={COMPUTATION}, binary={newer}")),
            "{error}"
        );
    }
}
