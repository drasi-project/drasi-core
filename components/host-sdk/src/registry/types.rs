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

//! Core types for plugin registry operations.

use serde::{Deserialize, Serialize};

/// Configuration for connecting to an OCI plugin registry.
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// Default registry prefix for short plugin names (e.g., "ghcr.io/drasi-project").
    pub default_registry: String,
    /// Authentication credentials.
    pub auth: RegistryAuth,
}

/// Authentication for an OCI registry.
#[derive(Debug, Clone)]
pub enum RegistryAuth {
    /// No authentication (public registries).
    Anonymous,
    /// Username/password or token-based authentication.
    Basic { username: String, password: String },
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            default_registry: "ghcr.io/drasi-project".to_string(),
            auth: RegistryAuth::Anonymous,
        }
    }
}

/// Version information from the host application, used for compatibility checks.
///
/// All version fields use semver format. Compatibility requires major.minor match
/// between host and plugin for sdk, core, and lib versions.
#[derive(Debug, Clone)]
pub struct HostVersionInfo {
    /// Version of drasi-plugin-sdk used by the host.
    pub sdk_version: String,
    /// Version of drasi-core used by the host.
    pub core_version: String,
    /// Version of drasi-lib used by the host.
    pub lib_version: String,
    /// Host's Rust target triple (e.g., "x86_64-unknown-linux-gnu").
    pub target_triple: String,
}

/// A fully resolved plugin reference, ready to download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPlugin {
    /// Full OCI reference with digest (e.g., "ghcr.io/drasi-project/source/postgres@sha256:abc123").
    pub reference: String,
    /// Plugin version (e.g., "0.1.8").
    pub version: String,
    /// SDK version of the plugin.
    pub sdk_version: String,
    /// Core version of the plugin.
    pub core_version: String,
    /// Lib version of the plugin.
    pub lib_version: String,
    /// OCI platform string (e.g., "linux/amd64").
    pub platform: String,
    /// SHA256 digest of the manifest.
    pub digest: String,
    /// Expected filename for the downloaded binary.
    pub filename: String,
}

/// Plugin metadata as stored in the OCI metadata layer (metadata.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMetadataJson {
    /// Crate name (e.g., "drasi-source-postgres").
    pub name: String,
    /// Plugin kind (e.g., "postgres").
    pub kind: String,
    /// Plugin type: "source", "reaction", or "bootstrap".
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Plugin version (e.g., "0.1.8").
    pub version: String,
    /// drasi-plugin-sdk version.
    pub sdk_version: String,
    /// drasi-core version.
    pub core_version: String,
    /// drasi-lib version.
    pub lib_version: String,
    /// Rust target triple.
    pub target_triple: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional license.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
}

/// A parsed plugin reference, broken into components.
#[derive(Debug, Clone)]
pub struct PluginReference {
    /// Full registry host (e.g., "ghcr.io").
    pub registry: String,
    /// Repository path (e.g., "drasi/source/postgres").
    pub repository: String,
    /// Tag or digest (e.g., "0.1.8" or "sha256:abc123").
    pub tag: Option<String>,
}

impl PluginReference {
    /// Parse a plugin reference string.
    ///
    /// Supports:
    /// - Full: `ghcr.io/drasi-project/source/postgres:0.1.8`
    /// - Short: `source/postgres:0.1.8` (uses default registry)
    /// - No tag: `source/postgres` (latest compatible)
    pub fn parse(reference: &str, default_registry: &str) -> anyhow::Result<Self> {
        let (ref_without_tag, tag) = if let Some(at_pos) = reference.rfind('@') {
            (
                &reference[..at_pos],
                Some(reference[at_pos + 1..].to_string()),
            )
        } else if let Some(colon_pos) = reference.rfind(':') {
            // Only treat as tag if the colon is after the last slash
            let last_slash = reference.rfind('/').unwrap_or(0);
            if colon_pos > last_slash {
                (
                    &reference[..colon_pos],
                    Some(reference[colon_pos + 1..].to_string()),
                )
            } else {
                (reference, None)
            }
        } else {
            (reference, None)
        };

        // Check if reference contains a registry (has a dot in the first path segment)
        let parts: Vec<&str> = ref_without_tag.splitn(2, '/').collect();
        let (registry, repository) = if parts.len() == 2 && parts[0].contains('.') {
            // Full reference: ghcr.io/drasi-project/source/postgres
            // Registry is first segment with dot, repository is the rest
            let first_slash = ref_without_tag.find('/').unwrap();
            let registry = &ref_without_tag[..first_slash];
            let repository = &ref_without_tag[first_slash + 1..];
            (registry.to_string(), repository.to_string())
        } else {
            // Short reference: source/postgres or drasi/source/postgres
            // Expand with default registry
            let (reg, ns) = if let Some(slash) = default_registry.find('/') {
                (
                    default_registry[..slash].to_string(),
                    default_registry[slash + 1..].to_string(),
                )
            } else {
                (default_registry.to_string(), String::new())
            };

            let repo = if ns.is_empty() {
                ref_without_tag.to_string()
            } else {
                format!("{}/{}", ns, ref_without_tag)
            };

            (reg, repo)
        };

        Ok(Self {
            registry,
            repository,
            tag,
        })
    }

    /// Convert to an OCI reference string.
    pub fn to_oci_reference(&self) -> String {
        match &self.tag {
            Some(tag) => format!("{}/{}:{}", self.registry, self.repository, tag),
            None => format!("{}/{}", self.registry, self.repository),
        }
    }
}

/// OCI annotation keys used for Drasi plugin metadata.
pub mod annotations {
    pub const PLUGIN_KIND: &str = "io.drasi.plugin.kind";
    pub const PLUGIN_TYPE: &str = "io.drasi.plugin.type";
    pub const SDK_VERSION: &str = "io.drasi.plugin.sdk-version";
    pub const CORE_VERSION: &str = "io.drasi.plugin.core-version";
    pub const LIB_VERSION: &str = "io.drasi.plugin.lib-version";
    pub const TARGET_TRIPLE: &str = "io.drasi.plugin.target-triple";
}

/// OCI media types for Drasi plugin artifacts.
pub mod media_types {
    pub const PLUGIN_BINARY: &str = "application/vnd.drasi.plugin.v1+binary";
    pub const PLUGIN_METADATA: &str = "application/vnd.drasi.plugin.v1+metadata";
    pub const PLUGIN_CONFIG: &str = "application/vnd.drasi.plugin.v1+config";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_reference() {
        let p =
            PluginReference::parse("ghcr.io/drasi-project/source/postgres:0.1.8", "ghcr.io/drasi-project")
                .unwrap();
        assert_eq!(p.registry, "ghcr.io");
        assert_eq!(p.repository, "drasi-project/source/postgres");
        assert_eq!(p.tag, Some("0.1.8".to_string()));
    }

    #[test]
    fn test_parse_short_reference() {
        let p = PluginReference::parse("source/postgres:0.1.8", "ghcr.io/drasi-project").unwrap();
        assert_eq!(p.registry, "ghcr.io");
        assert_eq!(p.repository, "drasi-project/source/postgres");
        assert_eq!(p.tag, Some("0.1.8".to_string()));
    }

    #[test]
    fn test_parse_no_tag() {
        let p = PluginReference::parse("source/postgres", "ghcr.io/drasi-project").unwrap();
        assert_eq!(p.registry, "ghcr.io");
        assert_eq!(p.repository, "drasi-project/source/postgres");
        assert_eq!(p.tag, None);
    }

    #[test]
    fn test_parse_third_party() {
        let p = PluginReference::parse(
            "ghcr.io/acme-corp/custom-source:1.0.0",
            "ghcr.io/drasi-project",
        )
        .unwrap();
        assert_eq!(p.registry, "ghcr.io");
        assert_eq!(p.repository, "acme-corp/custom-source");
        assert_eq!(p.tag, Some("1.0.0".to_string()));
    }

    #[test]
    fn test_to_oci_reference() {
        let p = PluginReference::parse("source/postgres:0.1.8", "ghcr.io/drasi-project").unwrap();
        assert_eq!(p.to_oci_reference(), "ghcr.io/drasi-project/source/postgres:0.1.8");
    }

    #[test]
    fn test_to_oci_reference_no_tag() {
        let p = PluginReference::parse("source/postgres", "ghcr.io/drasi-project").unwrap();
        assert_eq!(p.to_oci_reference(), "ghcr.io/drasi-project/source/postgres");
    }
}
