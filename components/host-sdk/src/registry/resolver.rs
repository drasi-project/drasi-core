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

//! Version resolver for finding compatible plugin versions from an OCI registry.

use crate::registry::oci::OciRegistryClient;
use crate::registry::platform::{target_triple_to_oci_platform, target_triple_to_arch_suffix, strip_arch_suffix};
use crate::registry::types::{
    HostVersionInfo, PluginReference, ResolvedPlugin, annotations,
};
use anyhow::{Context, Result, bail};
use log::{debug, info, warn};
use semver::Version;

/// Resolves plugin references to specific compatible versions.
pub struct PluginResolver<'a> {
    client: &'a OciRegistryClient,
    host_info: &'a HostVersionInfo,
}

impl<'a> PluginResolver<'a> {
    /// Create a new resolver.
    pub fn new(client: &'a OciRegistryClient, host_info: &'a HostVersionInfo) -> Self {
        Self { client, host_info }
    }

    /// Resolve a plugin reference to a specific compatible version.
    ///
    /// - If a tag is specified, validates compatibility and returns it.
    /// - If no tag, finds the latest version compatible with the host's SDK/core/lib versions.
    pub async fn resolve(
        &self,
        reference: &str,
        default_registry: &str,
    ) -> Result<ResolvedPlugin> {
        let parsed = PluginReference::parse(reference, default_registry)?;

        match &parsed.tag {
            Some(tag) => self.resolve_exact(&parsed, tag).await,
            None => self.resolve_latest_compatible(&parsed).await,
        }
    }

    /// Resolve an exact version tag — append platform suffix and validate compatibility.
    async fn resolve_exact(
        &self,
        parsed: &PluginReference,
        tag: &str,
    ) -> Result<ResolvedPlugin> {
        let arch_suffix = target_triple_to_arch_suffix(&self.host_info.target_triple)
            .context("unsupported platform — cannot determine architecture suffix")?;
        let platform_tag = format!("{}-{}", tag, arch_suffix);
        let full_ref = format!("{}/{}:{}", parsed.registry, parsed.repository, platform_tag);

        debug!("Resolving exact version: {} (platform tag: {})", tag, platform_tag);

        // Fetch annotations for compatibility check
        let annotations = self
            .client
            .fetch_manifest_annotations(&full_ref)
            .await
            .context("failed to fetch manifest annotations")?;

        // Validate compatibility
        self.check_compatibility(&annotations, &full_ref)?;

        // Get digest
        let digest = self
            .client
            .get_digest(&full_ref)
            .await
            .context("failed to get digest")?;

        // Derive filename from metadata
        let filename = self.derive_filename(&annotations);

        Ok(ResolvedPlugin {
            reference: format!(
                "{}/{}@{}",
                parsed.registry, parsed.repository, digest
            ),
            version: tag.to_string(),
            sdk_version: annotations
                .get(annotations::SDK_VERSION)
                .cloned()
                .unwrap_or_default(),
            core_version: annotations
                .get(annotations::CORE_VERSION)
                .cloned()
                .unwrap_or_default(),
            lib_version: annotations
                .get(annotations::LIB_VERSION)
                .cloned()
                .unwrap_or_default(),
            platform: target_triple_to_oci_platform(&self.host_info.target_triple)
                .map(|p| p.to_string())
                .unwrap_or_default(),
            digest,
            filename,
        })
    }

    /// Resolve the latest compatible version by listing tags and checking each.
    async fn resolve_latest_compatible(
        &self,
        parsed: &PluginReference,
    ) -> Result<ResolvedPlugin> {
        let base_ref = format!("{}/{}", parsed.registry, parsed.repository);

        info!("Resolving latest compatible version for {}...", base_ref);

        let arch_suffix = target_triple_to_arch_suffix(&self.host_info.target_triple)
            .context("unsupported platform — cannot determine architecture suffix")?;

        // List all tags
        let ref_for_tags = parsed.to_oci_reference();
        let tags = self
            .client
            .list_tags(&ref_for_tags)
            .await
            .context("failed to list tags")?;

        if tags.is_empty() {
            bail!("no tags found for {}", base_ref);
        }

        // Filter to tags matching our platform suffix, then strip the suffix for semver parsing
        let expected_suffix = format!("-{}", arch_suffix);
        let mut semver_tags: Vec<(Version, String)> = tags
            .into_iter()
            .filter_map(|tag| {
                // Only consider tags ending with our platform suffix
                let version_str = tag.strip_suffix(&expected_suffix)?;
                Version::parse(version_str).ok().and_then(|v| {
                    // Skip pre-release versions during auto-resolution
                    if v.pre.is_empty() {
                        Some((v, version_str.to_string()))
                    } else {
                        debug!("  Skipping pre-release tag: {}", version_str);
                        None
                    }
                })
            })
            .collect();

        semver_tags.sort_by(|a, b| b.0.cmp(&a.0)); // newest first

        if semver_tags.is_empty() {
            bail!(
                "no compatible tags found for {} on platform {}",
                base_ref,
                arch_suffix
            );
        }

        debug!(
            "Found {} semver tags for {}, checking compatibility (newest first)...",
            semver_tags.len(),
            arch_suffix
        );

        // Check each tag for compatibility (re-append suffix for the actual OCI reference)
        for (_version, version_str) in &semver_tags {
            let platform_tag = format!("{}-{}", version_str, arch_suffix);
            let full_ref = format!("{}:{}", base_ref, platform_tag);

            match self.client.fetch_manifest_annotations(&full_ref).await {
                Ok(ann) => {
                    if self.is_compatible(&ann) {
                        info!("Found compatible version: {} ({})", version_str, full_ref);

                        let digest = self
                            .client
                            .get_digest(&full_ref)
                            .await
                            .context("failed to get digest")?;

                        let filename = self.derive_filename(&ann);

                        return Ok(ResolvedPlugin {
                            reference: format!(
                                "{}/{}@{}",
                                parsed.registry, parsed.repository, digest
                            ),
                            version: version_str.clone(),
                            sdk_version: ann
                                .get(annotations::SDK_VERSION)
                                .cloned()
                                .unwrap_or_default(),
                            core_version: ann
                                .get(annotations::CORE_VERSION)
                                .cloned()
                                .unwrap_or_default(),
                            lib_version: ann
                                .get(annotations::LIB_VERSION)
                                .cloned()
                                .unwrap_or_default(),
                            platform: target_triple_to_oci_platform(
                                &self.host_info.target_triple,
                            )
                            .map(|p| p.to_string())
                            .unwrap_or_default(),
                            digest,
                            filename,
                        });
                    } else {
                        debug!(
                            "  {} — incompatible (sdk: {}, core: {}, lib: {})",
                            version_str,
                            ann.get(annotations::SDK_VERSION).unwrap_or(&"?".to_string()),
                            ann.get(annotations::CORE_VERSION).unwrap_or(&"?".to_string()),
                            ann.get(annotations::LIB_VERSION).unwrap_or(&"?".to_string()),
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to check {}: {}", full_ref, e);
                }
            }
        }

        bail!(
            "no compatible version found for {} on platform {}\n  Host versions: SDK {}, core {}, lib {}\n  Checked {} versions (newest first), none matched major.minor",
            base_ref,
            arch_suffix,
            self.host_info.sdk_version,
            self.host_info.core_version,
            self.host_info.lib_version,
            semver_tags.len()
        )
    }

    /// Check if a plugin's annotations indicate compatibility with the host.
    fn is_compatible(
        &self,
        ann: &std::collections::BTreeMap<String, String>,
    ) -> bool {
        let checks = [
            (annotations::SDK_VERSION, &self.host_info.sdk_version),
            (annotations::CORE_VERSION, &self.host_info.core_version),
            (annotations::LIB_VERSION, &self.host_info.lib_version),
        ];

        for (key, host_ver) in &checks {
            match ann.get(*key) {
                Some(plugin_ver) => {
                    if !major_minor_match(host_ver, plugin_ver) {
                        return false;
                    }
                }
                None => {
                    // Missing annotation — treat as incompatible
                    return false;
                }
            }
        }

        true
    }

    /// Validate compatibility and return an error with details if incompatible.
    fn check_compatibility(
        &self,
        ann: &std::collections::BTreeMap<String, String>,
        reference: &str,
    ) -> Result<()> {
        let checks = [
            ("SDK", annotations::SDK_VERSION, &self.host_info.sdk_version),
            ("core", annotations::CORE_VERSION, &self.host_info.core_version),
            ("lib", annotations::LIB_VERSION, &self.host_info.lib_version),
        ];

        let mut mismatches = Vec::new();

        for (name, key, host_ver) in &checks {
            if let Some(plugin_ver) = ann.get(*key) {
                if !major_minor_match(host_ver, plugin_ver) {
                    mismatches.push(format!(
                        "  {} version: host={}, plugin={} (major.minor mismatch)",
                        name, host_ver, plugin_ver
                    ));
                }
            } else {
                mismatches.push(format!(
                    "  {} version: missing annotation ({})",
                    name, key
                ));
            }
        }

        if !mismatches.is_empty() {
            bail!(
                "plugin {} is incompatible with this host:\n{}",
                reference,
                mismatches.join("\n")
            );
        }

        Ok(())
    }

    /// Derive the expected binary filename from annotations.
    fn derive_filename(
        &self,
        ann: &std::collections::BTreeMap<String, String>,
    ) -> String {
        let kind = ann
            .get(annotations::PLUGIN_KIND)
            .cloned()
            .unwrap_or_default();
        let plugin_type = ann
            .get(annotations::PLUGIN_TYPE)
            .cloned()
            .unwrap_or_default();

        let crate_name = format!("drasi_{}_{}", plugin_type, kind.replace('-', "_"));

        let target = &self.host_info.target_triple;
        let is_windows = target.contains("windows");
        let ext = if is_windows {
            "dll"
        } else if target.contains("apple") || target.contains("darwin") {
            "dylib"
        } else {
            "so"
        };

        let prefix = if is_windows { "" } else { "lib" };
        format!("{}{}.{}", prefix, crate_name, ext)
    }
}

/// Check if two semver strings match on major.minor.
fn major_minor_match(a: &str, b: &str) -> bool {
    match (Version::parse(a), Version::parse(b)) {
        (Ok(va), Ok(vb)) => va.major == vb.major && va.minor == vb.minor,
        _ => a == b, // fallback to exact match if not valid semver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_major_minor_match() {
        assert!(major_minor_match("0.3.1", "0.3.8"));
        assert!(major_minor_match("1.2.0", "1.2.99"));
        assert!(!major_minor_match("0.3.1", "0.4.0"));
        assert!(!major_minor_match("1.0.0", "2.0.0"));
    }

    #[test]
    fn test_major_minor_match_invalid() {
        assert!(major_minor_match("abc", "abc"));
        assert!(!major_minor_match("abc", "def"));
    }

    #[test]
    fn test_prerelease_tags_are_parseable_but_skipped() {
        // Pre-release tags should parse as valid semver
        let v = Version::parse("0.1.8-dev.1").unwrap();
        assert!(!v.pre.is_empty());

        // Stable versions have empty pre
        let v = Version::parse("0.1.8").unwrap();
        assert!(v.pre.is_empty());

        // Explicit pre-release references (with tag) should still resolve via resolve_exact
        let parsed = PluginReference::parse("source/postgres:0.1.8-dev.1", "ghcr.io/drasi-project");
        assert!(parsed.is_ok());
        let parsed = parsed.unwrap();
        assert_eq!(parsed.tag, Some("0.1.8-dev.1".to_string()));
    }
}
