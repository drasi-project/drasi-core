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
use crate::registry::platform::{
    fallback_arch_suffixes, target_triple_to_arch_suffix, target_triple_to_oci_platform,
};
use crate::registry::types::{annotations, HostVersionInfo, PluginReference, ResolvedPlugin};
use anyhow::{bail, Context, Result};
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
    pub async fn resolve(&self, reference: &str, default_registry: &str) -> Result<ResolvedPlugin> {
        let parsed = PluginReference::parse(reference, default_registry)?;

        match &parsed.tag {
            Some(tag) => self.resolve_exact(&parsed, tag).await,
            None => self.resolve_latest_compatible(&parsed).await,
        }
    }

    /// Resolve an exact version tag — append platform suffix and validate compatibility.
    async fn resolve_exact(&self, parsed: &PluginReference, tag: &str) -> Result<ResolvedPlugin> {
        let arch_suffix = target_triple_to_arch_suffix(&self.host_info.target_triple)
            .context("unsupported platform — cannot determine architecture suffix")?;

        // Build list of suffixes to try: primary first, then fallbacks
        let mut suffixes = vec![arch_suffix];
        suffixes.extend(fallback_arch_suffixes(&self.host_info.target_triple));

        let mut last_err = None;
        for suffix in &suffixes {
            let platform_tag = format!("{tag}-{suffix}");
            let full_ref = format!("{}/{}:{}", parsed.registry, parsed.repository, platform_tag);

            debug!("Resolving exact version: {tag} (platform tag: {platform_tag})");

            match self.client.fetch_manifest_annotations(&full_ref).await {
                Ok(annotations) => {
                    self.check_compatibility(&annotations, &full_ref)?;

                    let digest = self
                        .client
                        .get_digest(&full_ref)
                        .await
                        .context("failed to get digest")?;

                    let filename = self.derive_filename(&annotations)?;

                    return Ok(ResolvedPlugin {
                        reference: format!("{}/{}@{}", parsed.registry, parsed.repository, digest),
                        version: tag.to_string(),
                        sdk_version: annotations
                            .get(annotations::SDK_VERSION)
                            .cloned()
                            .unwrap_or_default(),
                        abi_family: annotations.get(annotations::ABI_FAMILY).cloned(),
                        abi_version: annotations.get(annotations::ABI_VERSION).cloned(),
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
                    });
                }
                Err(e) => {
                    if suffixes.len() > 1 {
                        debug!("Tag {platform_tag} not found, trying next suffix...");
                    }
                    last_err = Some(e);
                }
            }
        }

        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("no matching platform tag found"))
            .context("failed to fetch manifest annotations"))
    }

    /// Resolve the latest compatible version by listing tags and checking each.
    async fn resolve_latest_compatible(&self, parsed: &PluginReference) -> Result<ResolvedPlugin> {
        let base_ref = format!("{}/{}", parsed.registry, parsed.repository);

        info!("Resolving latest compatible version for {base_ref}...");

        let arch_suffix = target_triple_to_arch_suffix(&self.host_info.target_triple)
            .context("unsupported platform — cannot determine architecture suffix")?;

        // Build list of suffixes to try: primary first, then fallbacks
        let mut suffixes = vec![arch_suffix.clone()];
        suffixes.extend(fallback_arch_suffixes(&self.host_info.target_triple));

        // List all tags
        let ref_for_tags = parsed.to_oci_reference();
        let tags = self
            .client
            .list_tags(&ref_for_tags)
            .await
            .context("failed to list tags")?;

        if tags.is_empty() {
            bail!("no tags found for {base_ref}");
        }

        // Try each suffix in order
        for suffix in &suffixes {
            let expected_suffix = format!("-{suffix}");
            let mut semver_tags: Vec<(Version, String)> = tags
                .iter()
                .filter_map(|tag| {
                    let version_str = tag.strip_suffix(&expected_suffix)?;
                    Version::parse(version_str).ok().and_then(|v| {
                        if v.pre.is_empty() {
                            Some((v, version_str.to_string()))
                        } else {
                            debug!("  Skipping pre-release tag: {version_str}");
                            None
                        }
                    })
                })
                .collect();

            semver_tags.sort_by(|a, b| b.0.cmp(&a.0)); // newest first

            if semver_tags.is_empty() {
                if suffixes.len() > 1 {
                    debug!("No tags found for suffix {suffix}, trying next...");
                }
                continue;
            }

            debug!(
                "Found {} semver tags for {}, checking compatibility (newest first)...",
                semver_tags.len(),
                suffix
            );

            // Check each tag for compatibility
            for (_version, version_str) in &semver_tags {
                let platform_tag = format!("{version_str}-{suffix}");
                let full_ref = format!("{base_ref}:{platform_tag}");

                match self.client.fetch_manifest_annotations(&full_ref).await {
                    Ok(ann) => {
                        if self.is_compatible(&ann) {
                            info!("Found compatible version: {version_str} ({full_ref})");

                            let digest = self
                                .client
                                .get_digest(&full_ref)
                                .await
                                .context("failed to get digest")?;

                            let filename = self.derive_filename(&ann)?;

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
                                abi_family: ann.get(annotations::ABI_FAMILY).cloned(),
                                abi_version: ann.get(annotations::ABI_VERSION).cloned(),
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
                                ann.get(annotations::SDK_VERSION)
                                    .unwrap_or(&"?".to_string()),
                                ann.get(annotations::CORE_VERSION)
                                    .unwrap_or(&"?".to_string()),
                                ann.get(annotations::LIB_VERSION)
                                    .unwrap_or(&"?".to_string()),
                            );
                        }
                    }
                    Err(e) => {
                        warn!("Failed to check {full_ref}: {e}");
                    }
                }
            }
        }

        bail!(
            "no compatible version found for {} on platform {}\n  Host versions: SDK {}, core {}, lib {}\n  Checked tags across {} platform suffix(es)",
            base_ref,
            arch_suffix,
            self.host_info.sdk_version,
            self.host_info.core_version,
            self.host_info.lib_version,
            suffixes.len()
        )
    }

    /// Check if a plugin's annotations indicate compatibility with the host.
    fn is_compatible(&self, ann: &std::collections::BTreeMap<String, String>) -> bool {
        if ann.contains_key(annotations::ABI_FAMILY)
            || ann.contains_key(annotations::ABI_VERSION)
            || ann
                .get(annotations::PLUGIN_TYPE)
                .is_some_and(|kind| kind == "computation")
        {
            return native_compatible(ann, &self.host_info.target_triple);
        }
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
        if ann.contains_key(annotations::ABI_FAMILY)
            || ann.contains_key(annotations::ABI_VERSION)
            || ann
                .get(annotations::PLUGIN_TYPE)
                .is_some_and(|kind| kind == "computation")
        {
            anyhow::ensure!(
                native_compatible(ann, &self.host_info.target_triple),
                "plugin {reference} requires a supported explicit computation ABI and matching target; host ABI={}, target={}",
                drasi_computation_plugin_abi::ABI_VERSION,
                self.host_info.target_triple,
            );
            return Ok(());
        }
        let checks = [
            ("SDK", annotations::SDK_VERSION, &self.host_info.sdk_version),
            (
                "core",
                annotations::CORE_VERSION,
                &self.host_info.core_version,
            ),
            ("lib", annotations::LIB_VERSION, &self.host_info.lib_version),
        ];

        let mut mismatches = Vec::new();

        for (name, key, host_ver) in &checks {
            if let Some(plugin_ver) = ann.get(*key) {
                if !major_minor_match(host_ver, plugin_ver) {
                    mismatches.push(format!(
                        "  {name} version: host={host_ver}, plugin={plugin_ver} (major.minor mismatch)"
                    ));
                }
            } else {
                mismatches.push(format!("  {name} version: missing annotation ({key})"));
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
    fn derive_filename(&self, ann: &std::collections::BTreeMap<String, String>) -> Result<String> {
        if ann
            .get(annotations::ABI_FAMILY)
            .is_some_and(|family| family == "computation")
        {
            return native_filename(ann);
        }
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
        Ok(format!("{prefix}{crate_name}.{ext}"))
    }
}

fn native_filename(annotations: &std::collections::BTreeMap<String, String>) -> Result<String> {
    let filename = annotations
        .get(annotations::FILENAME)
        .context("native plugin artifact omitted its filename")?;
    let mut components = std::path::Path::new(filename).components();
    anyhow::ensure!(
        matches!(components.next(), Some(std::path::Component::Normal(_)))
            && components.next().is_none()
            && !filename.contains(['\\', '\0', ':']),
        "native plugin artifact filename must be a single file name",
    );
    Ok(filename.clone())
}

fn native_compatible(
    annotations: &std::collections::BTreeMap<String, String>,
    target: &str,
) -> bool {
    annotations
        .get(annotations::ABI_FAMILY)
        .is_some_and(|family| family == "computation")
        && annotations
            .get(annotations::PLUGIN_TYPE)
            .is_some_and(|kind| kind == "computation")
        && annotations
            .get(annotations::ABI_VERSION)
            .is_some_and(|version| version == drasi_computation_plugin_abi::ABI_VERSION)
        && annotations
            .get(annotations::TARGET_TRIPLE)
            .is_some_and(|plugin_target| plugin_target == target)
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
    fn native_artifact_filenames_preserve_custom_crate_names_but_not_paths() {
        let valid = std::collections::BTreeMap::from([(
            annotations::FILENAME.into(),
            "libdrasi_custom_native_fixture.dylib".into(),
        )]);
        assert_eq!(
            native_filename(&valid).unwrap(),
            "libdrasi_custom_native_fixture.dylib"
        );
        assert!(native_filename(&Default::default()).is_err());
        for filename in [
            "",
            "..",
            "../plugin.so",
            "/plugin.so",
            "nested/plugin.so",
            "C:plugin.dll",
            "plugin\0.so",
            "nested\\plugin.dll",
        ] {
            let annotations =
                std::collections::BTreeMap::from([(annotations::FILENAME.into(), filename.into())]);
            assert!(native_filename(&annotations).is_err(), "{filename:?}");
        }
    }

    #[test]
    fn computation_compatibility_uses_its_abi_not_legacy_rust_package_versions() {
        let target = "aarch64-apple-darwin";
        let mut annotations = std::collections::BTreeMap::from([
            (annotations::ABI_FAMILY.into(), "computation".into()),
            (
                annotations::ABI_VERSION.into(),
                drasi_computation_plugin_abi::ABI_VERSION.into(),
            ),
            (annotations::PLUGIN_TYPE.into(), "computation".into()),
            (annotations::TARGET_TRIPLE.into(), target.into()),
            (annotations::SDK_VERSION.into(), "99.1.0".into()),
            (annotations::CORE_VERSION.into(), "98.2.0".into()),
            (annotations::LIB_VERSION.into(), "97.3.0".into()),
        ]);
        assert!(native_compatible(&annotations, target));
        assert!(!native_compatible(&annotations, "x86_64-unknown-linux-gnu"));
        annotations.remove(annotations::ABI_VERSION);
        assert!(!native_compatible(&annotations, target));
        annotations.insert(annotations::ABI_VERSION.into(), "99.0.0".into());
        assert!(!native_compatible(&annotations, target));
        annotations.insert(
            annotations::ABI_VERSION.into(),
            drasi_computation_plugin_abi::ABI_VERSION.into(),
        );
        annotations.insert(annotations::ABI_FAMILY.into(), "unknown".into());
        assert!(!native_compatible(&annotations, target));
    }

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
