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

    /// Search for plugins matching a name across all type prefixes.
    ///
    /// If the reference already contains a type prefix (e.g., "source/postgres"),
    /// only that exact package is searched. If it's a bare name (e.g., "postgres"),
    /// all type prefixes are tried: source/, reaction/, bootstrap/.
    pub async fn search_plugins(&self, reference: &str) -> Result<Vec<PluginSearchResult>> {
        let has_type_prefix = reference.contains('/') 
            && !reference.contains('.');  // not a full registry URL

        let candidates: Vec<String> = if has_type_prefix {
            vec![reference.to_string()]
        } else {
            vec![
                format!("source/{}", reference),
                format!("reaction/{}", reference),
                format!("bootstrap/{}", reference),
            ]
        };

        let mut results = Vec::new();
        for candidate in &candidates {
            match self.list_tags(candidate).await {
                Ok(tags) => {
                    results.push(PluginSearchResult {
                        reference: candidate.clone(),
                        full_reference: self.expand_reference(candidate).unwrap_or_default(),
                        tags,
                    });
                }
                Err(_) => {
                    // Package doesn't exist or not accessible — skip
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
    /// Available tags.
    pub tags: Vec<String>,
}
