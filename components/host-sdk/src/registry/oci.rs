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

use crate::registry::types::{
    PluginMetadataJson, PluginReference, RegistryAuth, RegistryConfig,
    media_types,
};
use anyhow::{Context, Result, bail};
use log::info;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::Reference;
use std::path::{Path, PathBuf};

/// Well-known package name used as a plugin directory index.
/// Each tag in this package represents a registered plugin (e.g., "source.postgres").
const PLUGIN_DIRECTORY_PACKAGE: &str = ".drasi-plugin-directory";

/// OCI registry client for interacting with plugin artifact registries.
pub struct OciRegistryClient {
    client: oci_client::Client,
    config: RegistryConfig,
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
        }
    }

    /// Get the OCI auth credentials.
    fn auth(&self) -> oci_client::secrets::RegistryAuth {
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
        let oci_ref: Reference = parsed.to_oci_reference().parse()
            .context("invalid OCI reference")?;

        let response = self
            .client
            .list_tags(&oci_ref, &self.auth(), None, None)
            .await
            .context("failed to list tags")?;

        Ok(response.tags)
    }

    /// Fetch the manifest and annotations for a specific tag.
    pub async fn fetch_manifest_annotations(
        &self,
        reference: &str,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let oci_ref: Reference = reference.parse()
            .context("invalid OCI reference")?;

        let (manifest, _digest) = self
            .client
            .pull_manifest(&oci_ref, &self.auth())
            .await
            .context("failed to pull manifest")?;

        match manifest {
            oci_client::manifest::OciManifest::Image(img) => {
                Ok(img.annotations.unwrap_or_default())
            }
            oci_client::manifest::OciManifest::ImageIndex(_) => {
                bail!("expected image manifest, got image index; specify a platform-specific tag")
            }
        }
    }

    /// Fetch the plugin metadata JSON from the metadata layer.
    pub async fn fetch_metadata(&self, reference: &str) -> Result<PluginMetadataJson> {
        let oci_ref: Reference = reference.parse()
            .context("invalid OCI reference")?;

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
    /// Returns the path to the downloaded binary file.
    pub async fn download_plugin(
        &self,
        reference: &str,
        dest_dir: &Path,
        filename: &str,
    ) -> Result<PathBuf> {
        let oci_ref: Reference = reference.parse()
            .context("invalid OCI reference")?;

        info!("Downloading plugin from {}...", reference);

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

                return Ok(dest_path);
            }
        }

        bail!("no binary layer found in artifact")
    }

    /// Get the manifest digest for a reference.
    pub async fn get_digest(&self, reference: &str) -> Result<String> {
        let oci_ref: Reference = reference.parse()
            .context("invalid OCI reference")?;

        let (_manifest, digest) = self
            .client
            .pull_manifest(&oci_ref, &self.auth())
            .await
            .context("failed to pull manifest")?;

        Ok(digest)
    }

    /// Expand a short plugin reference to a full OCI reference.
    pub fn expand_reference(&self, reference: &str) -> Result<String> {
        let parsed = PluginReference::parse(reference, &self.config.default_registry)?;
        Ok(parsed.to_oci_reference())
    }

    /// Search for plugins matching a query.
    ///
    /// Discovers available plugins by scanning the `.drasi-plugin-directory` package
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
        let dir_oci_ref: Reference = dir_ref
            .parse()
            .context("invalid directory reference")?;
        let dir_response = self
            .client
            .list_tags(&dir_oci_ref, &self.auth(), None, None)
            .await
            .context(
                "failed to list plugin directory — directory package may not exist yet",
            )?;

        // Parse directory tags into (type, kind) pairs
        // Tags are formatted as "type.kind" (e.g., "source.postgres", "reaction.storedproc-mssql")
        let mut candidates: Vec<(String, String)> = Vec::new();
        for tag in &dir_response.tags {
            if let Some((ptype, kind)) = tag.split_once('.') {
                let plugin_ref = format!("{}/{}", ptype, kind);

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
            let plugin_ref = format!("{}/{}", ptype, kind);
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
