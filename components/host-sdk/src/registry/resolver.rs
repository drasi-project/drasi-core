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

use crate::loader::abi_versions_compatible;
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
    /// - If no tag, finds the latest version compatible with the host's package versions and ABI.
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

            match self
                .client
                .fetch_manifest_annotations_with_digest(&full_ref)
                .await
            {
                Ok((annotations, digest)) => {
                    self.check_compatibility(&annotations, &full_ref)?;

                    let filename = self.derive_filename(&annotations);

                    return Ok(ResolvedPlugin {
                        reference: format!("{}/{}@{}", parsed.registry, parsed.repository, digest),
                        version: tag.to_string(),
                        sdk_version: annotations
                            .get(annotations::SDK_VERSION)
                            .cloned()
                            .unwrap_or_default(),
                        abi_version: annotations
                            .get(annotations::ABI_VERSION)
                            .context("missing validated FFI ABI annotation")?
                            .clone(),
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

                match self
                    .client
                    .fetch_manifest_annotations_with_digest(&full_ref)
                    .await
                {
                    Ok((ann, digest)) => {
                        if self.is_compatible(&ann) {
                            info!("Found compatible version: {version_str} ({full_ref})");

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
                                abi_version: ann
                                    .get(annotations::ABI_VERSION)
                                    .context("missing validated FFI ABI annotation")?
                                    .clone(),
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
                                "  {} — incompatible (SDK package: {}, FFI ABI: {}, core: {}, lib: {})",
                                version_str,
                                ann.get(annotations::SDK_VERSION)
                                    .unwrap_or(&"?".to_string()),
                                ann.get(annotations::ABI_VERSION)
                                    .unwrap_or(&"<missing>".to_string()),
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
            "no compatible version found for {} on platform {}\n  Host versions: SDK package {}, FFI ABI {}, core {}, lib {}\n  Checked tags across {} platform suffix(es). Every candidate requires an explicit FFI ABI annotation; legacy artifacts without it must be republished with validated metadata.",
            base_ref,
            arch_suffix,
            self.host_info.sdk_version,
            self.host_info.abi_version(),
            self.host_info.core_version,
            self.host_info.lib_version,
            suffixes.len()
        )
    }

    /// Check if a plugin's annotations indicate compatibility with the host.
    fn is_compatible(&self, ann: &std::collections::BTreeMap<String, String>) -> bool {
        self.compatibility_mismatches(ann).is_empty()
    }

    /// Validate compatibility and return an error with details if incompatible.
    fn check_compatibility(
        &self,
        ann: &std::collections::BTreeMap<String, String>,
        reference: &str,
    ) -> Result<()> {
        let mismatches = self.compatibility_mismatches(ann);
        if !mismatches.is_empty() {
            bail!(
                "plugin {} is incompatible with this host:\n  SDK package: host={}, plugin={}\n  FFI ABI: host={}, plugin={}\n{}",
                reference,
                self.host_info.sdk_version,
                ann.get(annotations::SDK_VERSION).map(String::as_str).unwrap_or("<missing>"),
                self.host_info.abi_version(),
                ann.get(annotations::ABI_VERSION).map(String::as_str).unwrap_or("<missing>"),
                mismatches.join("\n")
            );
        }
        Ok(())
    }

    fn compatibility_mismatches(
        &self,
        ann: &std::collections::BTreeMap<String, String>,
    ) -> Vec<String> {
        let checks = [
            (
                "SDK package",
                annotations::SDK_VERSION,
                self.host_info.sdk_version.as_str(),
            ),
            (
                "FFI ABI",
                annotations::ABI_VERSION,
                self.host_info.abi_version(),
            ),
            (
                "core",
                annotations::CORE_VERSION,
                self.host_info.core_version.as_str(),
            ),
            (
                "lib",
                annotations::LIB_VERSION,
                self.host_info.lib_version.as_str(),
            ),
        ];

        let mut mismatches = Vec::new();

        for (name, key, host_ver) in &checks {
            if let Some(plugin_ver) = ann.get(*key) {
                let compatible = if *key == annotations::ABI_VERSION {
                    abi_versions_compatible(host_ver, plugin_ver)
                } else {
                    major_minor_match(host_ver, plugin_ver)
                };
                if !compatible {
                    mismatches.push(format!(
                        "  {name} version: host={host_ver}, plugin={plugin_ver} (major.minor mismatch)"
                    ));
                }
            } else {
                mismatches.push(format!("  {name} version: missing annotation ({key})"));
                if *key == annotations::ABI_VERSION {
                    mismatches.push(
                        "  Legacy artifacts require republishing with validated ABI metadata."
                            .to_string(),
                    );
                }
            }
        }

        mismatches
    }

    /// Derive the expected binary filename from annotations.
    fn derive_filename(&self, ann: &std::collections::BTreeMap<String, String>) -> String {
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
        format!("{prefix}{crate_name}.{ext}")
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
    use crate::registry::types::{RegistryAuth, RegistryConfig};
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    fn historical_host() -> HostVersionInfo {
        let mut host = HostVersionInfo::new("0.11.0", "0.5.8", "0.9.1", "aarch64-apple-darwin");
        host.abi_version = "0.13.0".to_string();
        host
    }

    fn plugin_annotations(abi: &str) -> BTreeMap<String, String> {
        [
            (annotations::SDK_VERSION, "0.11.3"),
            (annotations::ABI_VERSION, abi),
            (annotations::CORE_VERSION, "0.5.10"),
            (annotations::LIB_VERSION, "0.9.3"),
            (annotations::PLUGIN_TYPE, "source"),
            (annotations::PLUGIN_KIND, "mock"),
            (annotations::TARGET_TRIPLE, "aarch64-apple-darwin"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    struct TestRegistry {
        prefix: String,
        manifests: BTreeMap<String, (String, String)>,
        requests: Arc<Mutex<Vec<String>>>,
        stopped: Arc<AtomicBool>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl TestRegistry {
        fn new(artifacts: &[(&str, BTreeMap<String, String>)]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test registry");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let prefix = format!("{}/test", listener.local_addr().expect("registry address"));
            let manifests: BTreeMap<_, _> = artifacts
                .iter()
                .map(|(tag, annotations)| {
                    let body = serde_json::json!({
                        "schemaVersion": 2,
                        "mediaType": "application/vnd.oci.image.manifest.v1+json",
                        "config": {
                            "mediaType": "application/vnd.drasi.plugin.v1+config",
                            "digest": format!("sha256:{:x}", Sha256::digest(b"{}")),
                            "size": 2
                        },
                        "layers": [],
                        "annotations": annotations
                    })
                    .to_string();
                    let digest = format!("sha256:{:x}", Sha256::digest(body.as_bytes()));
                    (tag.to_string(), (body, digest))
                })
                .collect();
            let responses = manifests.clone();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded_requests = requests.clone();
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let server = thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("test registry accept: {error}"),
                    };
                    stream
                        .set_nonblocking(false)
                        .expect("blocking registry connection");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("registry read timeout");
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("registry request line");
                    let path = line
                        .split_whitespace()
                        .nth(1)
                        .expect("registry request path")
                        .to_string();
                    loop {
                        line.clear();
                        if reader
                            .read_line(&mut line)
                            .expect("registry request header")
                            == 0
                            || line == "\r\n"
                        {
                            break;
                        }
                    }
                    recorded_requests
                        .lock()
                        .expect("request log")
                        .push(path.clone());
                    let (status, content_type, body, digest) =
                        if path.starts_with("/v2/test/source/mock/tags/list") {
                            let tags: Vec<_> = if path.contains("last=") {
                                Vec::new()
                            } else {
                                responses.keys().cloned().collect()
                            };
                            (
                                "200 OK",
                                "application/json",
                                serde_json::json!({"name": "test/source/mock", "tags": tags})
                                    .to_string(),
                                String::new(),
                            )
                        } else if let Some((body, digest)) = path
                            .strip_prefix("/v2/test/source/mock/manifests/")
                            .and_then(|tag| responses.get(tag))
                        {
                            (
                                "200 OK",
                                "application/vnd.oci.image.manifest.v1+json",
                                body.clone(),
                                digest.clone(),
                            )
                        } else if path == "/v2/" {
                            (
                                "200 OK",
                                "application/json",
                                "{}".to_string(),
                                String::new(),
                            )
                        } else {
                            (
                                "404 Not Found",
                                "application/json",
                                r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"not found"}]}"#
                                    .to_string(),
                                String::new(),
                            )
                        };
                    write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nDocker-Content-Digest: {digest}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .expect("registry response");
                }
            });
            Self {
                prefix,
                manifests,
                requests,
                stopped,
                server: Some(server),
            }
        }

        fn client(&self) -> OciRegistryClient {
            OciRegistryClient::for_test_registry(RegistryConfig {
                default_registry: self.prefix.clone(),
                auth: RegistryAuth::Anonymous,
            })
        }
    }

    impl Drop for TestRegistry {
        fn drop(&mut self) {
            self.stopped.store(true, Ordering::Release);
            if let Err(error) = self.server.take().expect("registry server").join() {
                if thread::panicking() {
                    eprintln!("test registry thread failed: {error:?}");
                } else {
                    std::panic::resume_unwind(error);
                }
            }
        }
    }

    #[test]
    fn host_abi_comes_from_the_binary_loader_not_the_package_version() {
        let host = HostVersionInfo::new("0.11.0", "0.5.8", "0.9.1", "aarch64-apple-darwin");
        assert_eq!(host.abi_version(), drasi_plugin_sdk::ffi::FFI_SDK_VERSION);
        assert_ne!(host.abi_version(), host.sdk_version);
    }

    #[test]
    fn accepts_package_and_abi_patch_differences() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = historical_host();
        let resolver = PluginResolver::new(&client, &host);
        let ann = plugin_annotations("0.13.9");
        assert!(resolver.is_compatible(&ann));
        resolver
            .check_compatibility(&ann, "test")
            .expect("compatible patches");
    }

    #[test]
    fn rejects_each_package_major_minor_mismatch() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = historical_host();
        let resolver = PluginResolver::new(&client, &host);
        for (key, version) in [
            (annotations::SDK_VERSION, "1.11.3"),
            (annotations::SDK_VERSION, "0.12.0"),
            (annotations::CORE_VERSION, "1.5.10"),
            (annotations::CORE_VERSION, "0.6.0"),
            (annotations::LIB_VERSION, "1.9.3"),
            (annotations::LIB_VERSION, "0.10.0"),
        ] {
            let mut ann = plugin_annotations("0.13.0");
            ann.insert(key.to_string(), version.to_string());
            assert!(!resolver.is_compatible(&ann), "{key}={version}");
            assert!(resolver.check_compatibility(&ann, "test").is_err());
        }
    }

    #[test]
    fn rejects_each_missing_required_version_annotation() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = historical_host();
        let resolver = PluginResolver::new(&client, &host);
        for key in [
            annotations::SDK_VERSION,
            annotations::ABI_VERSION,
            annotations::CORE_VERSION,
            annotations::LIB_VERSION,
        ] {
            let mut ann = plugin_annotations("0.13.0");
            ann.remove(key);
            assert!(!resolver.is_compatible(&ann), "{key}");
            let error = resolver
                .check_compatibility(&ann, "test")
                .expect_err("missing annotation");
            assert!(error.to_string().contains(key), "{error}");
        }
    }

    #[test]
    fn rejects_abi_mismatches_and_invalid_versions_even_when_packages_match() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = historical_host();
        let resolver = PluginResolver::new(&client, &host);
        for abi in ["0.14.0", "1.13.0", "invalid", "0.13", ""] {
            let ann = plugin_annotations(abi);
            assert!(!resolver.is_compatible(&ann), "{abi}");
            let error = resolver
                .check_compatibility(&ann, "test")
                .expect_err("ABI mismatch");
            assert!(error.to_string().contains("FFI ABI"), "{error}");
            assert!(
                error
                    .to_string()
                    .contains("SDK package: host=0.11.0, plugin=0.11.3"),
                "{error}"
            );
        }
        assert!(!abi_versions_compatible("invalid", "invalid"));
    }

    #[tokio::test]
    async fn floating_resolution_skips_incompatible_and_legacy_artifacts() {
        let mut legacy = plugin_annotations("0.13.0");
        legacy.remove(annotations::ABI_VERSION);
        let registry = TestRegistry::new(&[
            ("0.2.10-darwin-arm64", plugin_annotations("0.13.7")),
            ("0.2.12-darwin-arm64", plugin_annotations("0.14.0")),
            ("0.2.13-darwin-arm64", legacy),
            ("0.2.14-dev.1-darwin-arm64", plugin_annotations("0.13.0")),
            ("9.0.0-linux-arm64", plugin_annotations("0.13.0")),
        ]);
        let client = registry.client();
        let host = historical_host();
        let resolved = PluginResolver::new(&client, &host)
            .resolve("source/mock", &registry.prefix)
            .await
            .expect("older compatible artifact");
        assert_eq!(resolved.version, "0.2.10");
        assert_eq!(resolved.sdk_version, "0.11.3");
        assert_eq!(resolved.abi_version, "0.13.7");
        assert_eq!(resolved.platform, "darwin/arm64");
        assert_eq!(resolved.filename, "libdrasi_source_mock.dylib");
        let digest = &registry.manifests["0.2.10-darwin-arm64"].1;
        assert_eq!(&resolved.digest, digest);
        assert!(resolved.reference.ends_with(digest));
        let requests = registry.requests.lock().expect("requests");
        assert_eq!(
            requests
                .iter()
                .filter(|path| path.ends_with("/manifests/0.2.10-darwin-arm64"))
                .count(),
            1,
            "the selected digest must come from the manifest whose annotations were checked"
        );
        assert!(!requests
            .iter()
            .any(|path| path.contains("/manifests/9.0.0-")));
        assert!(!requests
            .iter()
            .any(|path| path.contains("/manifests/0.2.14-")));
    }

    #[tokio::test]
    async fn explicit_incompatible_pin_fails_during_resolution() {
        let registry = TestRegistry::new(&[
            ("0.2.10-darwin-arm64", plugin_annotations("0.13.0")),
            ("0.2.12-darwin-arm64", plugin_annotations("0.14.0")),
        ]);
        let client = registry.client();
        let host = historical_host();
        let resolver = PluginResolver::new(&client, &host);
        let error = resolver
            .resolve("source/mock:0.2.12", &registry.prefix)
            .await
            .expect_err("explicit ABI mismatch");
        assert!(
            error
                .to_string()
                .contains("FFI ABI: host=0.13.0, plugin=0.14.0"),
            "{error}"
        );
        assert!(
            error
                .to_string()
                .contains("SDK package: host=0.11.0, plugin=0.11.3"),
            "{error}"
        );
        let compatible = resolver
            .resolve("source/mock:0.2.10", &registry.prefix)
            .await
            .expect("compatible explicit pin");
        assert_eq!(compatible.abi_version, "0.13.0");
    }

    #[tokio::test]
    async fn legacy_pin_and_floating_reference_without_compatible_candidates_fail() {
        let mut legacy = plugin_annotations("0.13.0");
        legacy.remove(annotations::ABI_VERSION);
        let registry = TestRegistry::new(&[
            ("0.2.10-darwin-arm64", legacy),
            ("0.2.12-darwin-arm64", plugin_annotations("0.14.0")),
        ]);
        let client = registry.client();
        let host = historical_host();
        let resolver = PluginResolver::new(&client, &host);
        let pinned = resolver
            .resolve("source/mock:0.2.10", &registry.prefix)
            .await
            .expect_err("legacy pin");
        assert!(
            pinned.to_string().contains(annotations::ABI_VERSION),
            "{pinned}"
        );
        let floating = resolver
            .resolve("source/mock", &registry.prefix)
            .await
            .expect_err("no compatible artifact");
        assert!(
            floating.to_string().contains("no compatible version found"),
            "{floating}"
        );
        assert!(
            floating.to_string().contains("FFI ABI 0.13.0"),
            "{floating}"
        );
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
