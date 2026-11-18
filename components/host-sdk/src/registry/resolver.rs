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

use crate::loader::{ordinary_abi_compatible, COMPUTATION_ABI_FAMILY, ORDINARY_ABI_FAMILY};
use crate::registry::oci::OciRegistryClient;
use crate::registry::platform::{
    fallback_arch_suffixes, target_triple_to_arch_suffix, target_triple_to_oci_platform,
};
use crate::registry::types::{annotations, HostVersionInfo, PluginReference, ResolvedPlugin};
use anyhow::{bail, Context, Result};
use log::{debug, info, warn};
use semver::Version;
use std::collections::BTreeMap;

/// Resolves plugin references to specific compatible versions.
///
/// Compatibility is decided only from the artifact's declared manifest
/// annotations, before anything is downloaded: every artifact must declare its
/// ABI family and that family's ABI version, which are checked with the same
/// rule the binary loader applies. Package versions never stand in for an ABI.
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
    /// - If no tag, finds the latest version compatible with the host's declared ABIs
    ///   (and, for the ordinary family, its package versions).
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
                    return self.resolved(parsed, tag, &annotations, digest);
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

                // The digest returned with the checked annotations is the one
                // resolved: a tag moved after the check cannot substitute
                // another manifest.
                match self
                    .client
                    .fetch_manifest_annotations_with_digest(&full_ref)
                    .await
                {
                    Ok((ann, digest)) => {
                        let mismatches = self.compatibility_mismatches(&ann);
                        if mismatches.is_empty() {
                            info!("Found compatible version: {version_str} ({full_ref})");
                            return self.resolved(parsed, version_str, &ann, digest);
                        }
                        debug!("  {version_str} — incompatible:\n{}", mismatches.join("\n"));
                    }
                    Err(e) => {
                        warn!("Failed to check {full_ref}: {e}");
                    }
                }
            }
        }

        bail!(
            "no compatible version found for {} on platform {}\n  Host versions: SDK package {}, FFI ABI {}, computation ABI {}, core {}, lib {}\n  Checked tags across {} platform suffix(es). Every candidate must declare its ABI family and ABI version; artifacts without them must be republished with validated metadata.",
            base_ref,
            arch_suffix,
            self.host_info.sdk_version,
            self.host_info.abi_version(),
            self.host_info.computation_abi_version(),
            self.host_info.core_version,
            self.host_info.lib_version,
            suffixes.len()
        )
    }

    fn resolved(
        &self,
        parsed: &PluginReference,
        version: &str,
        ann: &BTreeMap<String, String>,
        digest: String,
    ) -> Result<ResolvedPlugin> {
        let declared = |key: &str| -> Result<String> {
            ann.get(key)
                .cloned()
                .with_context(|| format!("missing validated annotation {key}"))
        };
        Ok(ResolvedPlugin {
            reference: format!("{}/{}@{}", parsed.registry, parsed.repository, digest),
            version: version.to_string(),
            sdk_version: ann
                .get(annotations::SDK_VERSION)
                .cloned()
                .unwrap_or_default(),
            abi_family: declared(annotations::ABI_FAMILY)?,
            abi_version: declared(annotations::ABI_VERSION)?,
            core_version: ann
                .get(annotations::CORE_VERSION)
                .cloned()
                .unwrap_or_default(),
            lib_version: ann
                .get(annotations::LIB_VERSION)
                .cloned()
                .unwrap_or_default(),
            platform: target_triple_to_oci_platform(&self.host_info.target_triple)
                .map(|p| p.to_string())
                .unwrap_or_default(),
            filename: self.derive_filename(ann)?,
            digest,
        })
    }

    /// Check if a plugin's annotations indicate compatibility with the host.
    fn is_compatible(&self, ann: &BTreeMap<String, String>) -> bool {
        self.compatibility_mismatches(ann).is_empty()
    }

    /// Validate compatibility and return an error with details if incompatible.
    fn check_compatibility(&self, ann: &BTreeMap<String, String>, reference: &str) -> Result<()> {
        let mismatches = self.compatibility_mismatches(ann);
        if mismatches.is_empty() {
            return Ok(());
        }
        let declared = |key: &str| ann.get(key).map(String::as_str).unwrap_or("<missing>");
        let abi = if ann.get(annotations::ABI_FAMILY).map(String::as_str)
            == Some(COMPUTATION_ABI_FAMILY)
        {
            format!(
                "computation ABI: host={}, plugin={}",
                self.host_info.computation_abi_version(),
                declared(annotations::ABI_VERSION)
            )
        } else {
            format!(
                "SDK package: host={}, plugin={}\n  FFI ABI: host={}, plugin={}",
                self.host_info.sdk_version,
                declared(annotations::SDK_VERSION),
                self.host_info.abi_version(),
                declared(annotations::ABI_VERSION)
            )
        };
        bail!(
            "plugin {reference} is incompatible with this host:\n  ABI family: {}\n  {abi}\n{}",
            declared(annotations::ABI_FAMILY),
            mismatches.join("\n")
        )
    }

    fn compatibility_mismatches(&self, ann: &BTreeMap<String, String>) -> Vec<String> {
        let plugin_type = ann.get(annotations::PLUGIN_TYPE).map(String::as_str);
        let mut mismatches = Vec::new();
        match ann.get(annotations::ABI_FAMILY).map(String::as_str) {
            None => mismatches.push(format!(
                "  ABI family: missing annotation ({}); artifacts must declare their plugin ABI family",
                annotations::ABI_FAMILY
            )),
            Some(ORDINARY_ABI_FAMILY) => {
                if plugin_type == Some("computation") {
                    mismatches.push(
                        "  plugin type computation does not belong to the ordinary ABI family"
                            .to_string(),
                    );
                }
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
                for (name, key, host_ver) in checks {
                    match ann.get(key) {
                        Some(plugin_ver) => {
                            let compatible = if key == annotations::ABI_VERSION {
                                ordinary_abi_compatible(plugin_ver)
                            } else {
                                major_minor_match(host_ver, plugin_ver)
                            };
                            if !compatible {
                                mismatches.push(format!(
                                    "  {name} version: host={host_ver}, plugin={plugin_ver} (major.minor mismatch)"
                                ));
                            }
                        }
                        None => mismatches.push(format!(
                            "  {name} version: missing annotation ({key})"
                        )),
                    }
                }
            }
            Some(COMPUTATION_ABI_FAMILY) => {
                if plugin_type != Some("computation") {
                    mismatches.push(format!(
                        "  plugin type {} does not belong to the computation ABI family",
                        plugin_type.unwrap_or("<missing>")
                    ));
                }
                match ann.get(annotations::ABI_VERSION) {
                    Some(version) if drasi_computation_plugin_abi::accepts_abi_version(version) => {}
                    Some(version) => mismatches.push(format!(
                        "  computation ABI version: host={}, plugin={version} (requires the same major and an older or equal minor)",
                        self.host_info.computation_abi_version()
                    )),
                    None => mismatches.push(format!(
                        "  computation ABI version: missing annotation ({})",
                        annotations::ABI_VERSION
                    )),
                }
                let host_target = &self.host_info.target_triple;
                match ann.get(annotations::TARGET_TRIPLE) {
                    Some(target) if target == host_target => {}
                    Some(target) => mismatches.push(format!(
                        "  target: host={host_target}, plugin={target}"
                    )),
                    None => mismatches.push(format!(
                        "  target: missing annotation ({})",
                        annotations::TARGET_TRIPLE
                    )),
                }
            }
            Some(family) => mismatches.push(format!("  ABI family: unsupported family {family}")),
        }
        mismatches
    }

    /// Derive the expected binary filename from annotations.
    fn derive_filename(&self, ann: &BTreeMap<String, String>) -> Result<String> {
        if ann.get(annotations::ABI_FAMILY).map(String::as_str) == Some(COMPUTATION_ABI_FAMILY) {
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

fn native_filename(annotations: &BTreeMap<String, String>) -> Result<String> {
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

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

    const TARGET: &str = "aarch64-apple-darwin";
    const ORDINARY_ABI: &str = drasi_plugin_sdk::ffi::metadata::FFI_SDK_VERSION;
    const COMPUTATION_ABI: &str = drasi_computation_plugin_abi::ABI_VERSION;

    fn host() -> HostVersionInfo {
        HostVersionInfo {
            sdk_version: "0.11.0".into(),
            core_version: "0.5.8".into(),
            lib_version: "0.9.1".into(),
            target_triple: TARGET.into(),
        }
    }

    /// The ordinary ABI with its major/minor/patch adjusted.
    fn ordinary_abi(major: i64, minor: i64, patch: u64) -> String {
        let host = Version::parse(ORDINARY_ABI).expect("host ABI");
        format!(
            "{}.{}.{}",
            host.major as i64 + major,
            host.minor as i64 + minor,
            host.patch + patch
        )
    }

    fn plugin_annotations(abi: &str) -> BTreeMap<String, String> {
        [
            (annotations::ABI_FAMILY, ORDINARY_ABI_FAMILY),
            (annotations::SDK_VERSION, "0.11.3"),
            (annotations::ABI_VERSION, abi),
            (annotations::CORE_VERSION, "0.5.10"),
            (annotations::LIB_VERSION, "0.9.3"),
            (annotations::PLUGIN_TYPE, "source"),
            (annotations::PLUGIN_KIND, "mock"),
            (annotations::TARGET_TRIPLE, TARGET),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    fn native_annotations(abi: &str) -> BTreeMap<String, String> {
        [
            (annotations::ABI_FAMILY, COMPUTATION_ABI_FAMILY),
            (annotations::ABI_VERSION, abi),
            (annotations::PLUGIN_TYPE, "computation"),
            (annotations::PLUGIN_KIND, "counter"),
            (annotations::TARGET_TRIPLE, TARGET),
            (
                annotations::FILENAME,
                "libdrasi_computation_source_counter.dylib",
            ),
            // Rust package versions are irrelevant to the native ABI.
            (annotations::SDK_VERSION, "99.1.0"),
            (annotations::CORE_VERSION, "98.2.0"),
            (annotations::LIB_VERSION, "97.3.0"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    /// Minimal OCI distribution endpoint serving manifests for one repository.
    struct TestRegistry {
        prefix: String,
        manifests: BTreeMap<String, (String, String)>,
        requests: Arc<Mutex<Vec<String>>>,
        stopped: Arc<AtomicBool>,
        server: Option<thread::JoinHandle<()>>,
    }

    impl TestRegistry {
        fn new(repository: &str, artifacts: &[(&str, BTreeMap<String, String>)]) -> Self {
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
            let tags_path = format!("/v2/test/{repository}/tags/list");
            let manifests_path = format!("/v2/test/{repository}/manifests/");
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded_requests = requests.clone();
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            let repository = repository.to_string();
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
                    let (status, content_type, body, digest) = if path.starts_with(&tags_path) {
                        let tags: Vec<_> = if path.contains("last=") {
                            Vec::new()
                        } else {
                            responses.keys().cloned().collect()
                        };
                        (
                            "200 OK",
                            "application/json",
                            serde_json::json!({"name": format!("test/{repository}"), "tags": tags})
                                .to_string(),
                            String::new(),
                        )
                    } else if let Some((body, digest)) = path
                        .strip_prefix(&manifests_path)
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

        fn manifest_requests(&self, tag: &str) -> usize {
            self.requests
                .lock()
                .expect("requests")
                .iter()
                .filter(|path| path.ends_with(&format!("/manifests/{tag}")))
                .count()
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
    fn host_abis_come_from_the_binary_loaders_not_package_versions() {
        let host = host();
        assert_eq!(host.abi_version(), ORDINARY_ABI);
        assert_eq!(host.computation_abi_version(), COMPUTATION_ABI);
        assert_ne!(host.abi_version(), host.sdk_version);
    }

    #[test]
    fn accepts_package_and_abi_patch_differences() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        let ann = plugin_annotations(&ordinary_abi(0, 0, 9));
        assert!(resolver.is_compatible(&ann));
        resolver
            .check_compatibility(&ann, "test")
            .expect("compatible patches");
    }

    #[test]
    fn rejects_each_package_major_minor_mismatch() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        for (key, version) in [
            (annotations::SDK_VERSION, "1.11.3"),
            (annotations::SDK_VERSION, "0.12.0"),
            (annotations::CORE_VERSION, "1.5.10"),
            (annotations::CORE_VERSION, "0.6.0"),
            (annotations::LIB_VERSION, "1.9.3"),
            (annotations::LIB_VERSION, "0.10.0"),
        ] {
            let mut ann = plugin_annotations(ORDINARY_ABI);
            ann.insert(key.to_string(), version.to_string());
            assert!(!resolver.is_compatible(&ann), "{key}={version}");
            assert!(resolver.check_compatibility(&ann, "test").is_err());
        }
    }

    #[test]
    fn rejects_each_missing_required_declaration() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        for key in [
            annotations::ABI_FAMILY,
            annotations::SDK_VERSION,
            annotations::ABI_VERSION,
            annotations::CORE_VERSION,
            annotations::LIB_VERSION,
        ] {
            let mut ann = plugin_annotations(ORDINARY_ABI);
            ann.remove(key);
            assert!(!resolver.is_compatible(&ann), "{key}");
            let error = resolver
                .check_compatibility(&ann, "test")
                .expect_err("missing annotation");
            assert!(error.to_string().contains(key), "{error}");
        }
        for key in [
            annotations::ABI_FAMILY,
            annotations::ABI_VERSION,
            annotations::TARGET_TRIPLE,
        ] {
            let mut ann = native_annotations(COMPUTATION_ABI);
            ann.remove(key);
            let error = resolver
                .check_compatibility(&ann, "test")
                .expect_err("missing native annotation");
            assert!(error.to_string().contains(key), "{error}");
        }
    }

    #[test]
    fn rejects_abi_mismatches_and_invalid_versions_even_when_packages_match() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        for abi in [
            ordinary_abi(0, 1, 0),
            ordinary_abi(0, -1, 0),
            ordinary_abi(1, 0, 0),
            "invalid".to_string(),
            "0.19".to_string(),
            String::new(),
        ] {
            let ann = plugin_annotations(&abi);
            assert!(!resolver.is_compatible(&ann), "{abi}");
            let error = resolver
                .check_compatibility(&ann, "test")
                .expect_err("ABI mismatch");
            assert!(
                error
                    .to_string()
                    .contains(&format!("FFI ABI: host={ORDINARY_ABI}, plugin={abi}")),
                "{error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("SDK package: host=0.11.0, plugin=0.11.3"),
                "{error}"
            );
        }
    }

    #[test]
    fn families_are_declared_never_inferred_from_type_or_version() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        // An ordinary-looking artifact with no family is not assumed ordinary.
        let mut ann = plugin_annotations(ORDINARY_ABI);
        ann.remove(annotations::ABI_FAMILY);
        assert!(!resolver.is_compatible(&ann));
        // A native ABI declared under the ordinary family (or vice versa) is rejected.
        let mut ann = native_annotations(COMPUTATION_ABI);
        ann.insert(annotations::ABI_FAMILY.into(), ORDINARY_ABI_FAMILY.into());
        assert!(!resolver.is_compatible(&ann));
        let mut ann = plugin_annotations(ORDINARY_ABI);
        ann.insert(
            annotations::ABI_FAMILY.into(),
            COMPUTATION_ABI_FAMILY.into(),
        );
        assert!(!resolver.is_compatible(&ann));
        let mut ann = native_annotations(COMPUTATION_ABI);
        ann.insert(annotations::ABI_FAMILY.into(), "unknown".into());
        assert!(!resolver.is_compatible(&ann));
    }

    #[test]
    fn computation_compatibility_uses_the_loader_rule_not_rust_package_versions() {
        let client = OciRegistryClient::new(RegistryConfig::default());
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        let abi = Version::parse(COMPUTATION_ABI).expect("native ABI");
        let patch = format!("{}.{}.{}", abi.major, abi.minor, abi.patch + 3);
        for accepted in [COMPUTATION_ABI, patch.as_str()] {
            let ann = native_annotations(accepted);
            assert!(resolver.is_compatible(&ann), "{accepted}");
            assert_eq!(
                resolver.derive_filename(&ann).unwrap(),
                "libdrasi_computation_source_counter.dylib"
            );
        }
        for rejected in [
            format!("{}.{}.0", abi.major, abi.minor + 1),
            format!("{}.0.0", abi.major + 1),
            format!("{}.0.0", abi.major - 1),
            "invalid".to_string(),
        ] {
            let ann = native_annotations(&rejected);
            let error = resolver
                .check_compatibility(&ann, "test")
                .expect_err("incompatible native ABI");
            assert!(
                error.to_string().contains(&format!(
                    "computation ABI: host={COMPUTATION_ABI}, plugin={rejected}"
                )),
                "{error}"
            );
        }
        let mut other_target = native_annotations(COMPUTATION_ABI);
        other_target.insert(
            annotations::TARGET_TRIPLE.into(),
            "x86_64-unknown-linux-gnu".into(),
        );
        assert!(!resolver.is_compatible(&other_target));
    }

    #[tokio::test]
    async fn floating_resolution_skips_incompatible_and_legacy_artifacts() {
        let mut legacy = plugin_annotations(ORDINARY_ABI);
        legacy.remove(annotations::ABI_VERSION);
        let mut undeclared_family = plugin_annotations(ORDINARY_ABI);
        undeclared_family.remove(annotations::ABI_FAMILY);
        let compatible_abi = ordinary_abi(0, 0, 7);
        let registry = TestRegistry::new(
            "source/mock",
            &[
                ("0.2.10-darwin-arm64", plugin_annotations(&compatible_abi)),
                (
                    "0.2.12-darwin-arm64",
                    plugin_annotations(&ordinary_abi(0, 1, 0)),
                ),
                ("0.2.13-darwin-arm64", legacy),
                ("0.2.14-darwin-arm64", undeclared_family),
                (
                    "0.2.15-dev.1-darwin-arm64",
                    plugin_annotations(ORDINARY_ABI),
                ),
                ("9.0.0-linux-arm64", plugin_annotations(ORDINARY_ABI)),
            ],
        );
        let client = registry.client();
        let host = host();
        let resolved = PluginResolver::new(&client, &host)
            .resolve("source/mock", &registry.prefix)
            .await
            .expect("older compatible artifact");
        assert_eq!(resolved.version, "0.2.10");
        assert_eq!(resolved.sdk_version, "0.11.3");
        assert_eq!(resolved.abi_family, ORDINARY_ABI_FAMILY);
        assert_eq!(resolved.abi_version, compatible_abi);
        assert_eq!(resolved.platform, "darwin/arm64");
        assert_eq!(resolved.filename, "libdrasi_source_mock.dylib");
        let digest = &registry.manifests["0.2.10-darwin-arm64"].1;
        assert_eq!(&resolved.digest, digest);
        assert!(resolved.reference.ends_with(digest));
        assert_eq!(
            registry.manifest_requests("0.2.10-darwin-arm64"),
            1,
            "the selected digest must come from the manifest whose annotations were checked"
        );
        let requests = registry.requests.lock().expect("requests");
        assert!(!requests
            .iter()
            .any(|path| path.contains("/manifests/9.0.0-")));
        assert!(!requests
            .iter()
            .any(|path| path.contains("/manifests/0.2.15-")));
    }

    #[tokio::test]
    async fn floating_native_resolution_skips_incompatible_native_abis() {
        let abi = Version::parse(COMPUTATION_ABI).expect("native ABI");
        let newer_minor = format!("{}.{}.0", abi.major, abi.minor + 1);
        let mut undeclared = native_annotations(COMPUTATION_ABI);
        undeclared.remove(annotations::ABI_VERSION);
        let registry = TestRegistry::new(
            "computation/counter",
            &[
                ("1.0.0-darwin-arm64", native_annotations(COMPUTATION_ABI)),
                ("1.1.0-darwin-arm64", native_annotations(&newer_minor)),
                ("1.2.0-darwin-arm64", undeclared),
            ],
        );
        let client = registry.client();
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        let resolved = resolver
            .resolve("computation/counter", &registry.prefix)
            .await
            .expect("compatible native artifact");
        assert_eq!(resolved.version, "1.0.0");
        assert_eq!(resolved.abi_family, COMPUTATION_ABI_FAMILY);
        assert_eq!(resolved.abi_version, COMPUTATION_ABI);
        assert_eq!(
            resolved.filename,
            "libdrasi_computation_source_counter.dylib"
        );
        assert_eq!(resolved.digest, registry.manifests["1.0.0-darwin-arm64"].1);
        let error = resolver
            .resolve("computation/counter:1.1.0", &registry.prefix)
            .await
            .expect_err("explicit pin to a newer native minor");
        assert!(
            error.to_string().contains(&format!(
                "computation ABI: host={COMPUTATION_ABI}, plugin={newer_minor}"
            )),
            "{error}"
        );
    }

    #[tokio::test]
    async fn explicit_incompatible_pin_fails_during_resolution() {
        let incompatible = ordinary_abi(0, 1, 0);
        let registry = TestRegistry::new(
            "source/mock",
            &[
                ("0.2.10-darwin-arm64", plugin_annotations(ORDINARY_ABI)),
                ("0.2.12-darwin-arm64", plugin_annotations(&incompatible)),
            ],
        );
        let client = registry.client();
        let host = host();
        let resolver = PluginResolver::new(&client, &host);
        let error = resolver
            .resolve("source/mock:0.2.12", &registry.prefix)
            .await
            .expect_err("explicit ABI mismatch");
        assert!(
            error.to_string().contains(&format!(
                "FFI ABI: host={ORDINARY_ABI}, plugin={incompatible}"
            )),
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
        assert_eq!(compatible.abi_version, ORDINARY_ABI);
        assert_eq!(registry.manifest_requests("0.2.10-darwin-arm64"), 1);
    }

    #[tokio::test]
    async fn legacy_pin_and_floating_reference_without_compatible_candidates_fail() {
        let mut legacy = plugin_annotations(ORDINARY_ABI);
        legacy.remove(annotations::ABI_VERSION);
        let registry = TestRegistry::new(
            "source/mock",
            &[
                ("0.2.10-darwin-arm64", legacy),
                (
                    "0.2.12-darwin-arm64",
                    plugin_annotations(&ordinary_abi(0, 1, 0)),
                ),
            ],
        );
        let client = registry.client();
        let host = host();
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
            floating
                .to_string()
                .contains(&format!("FFI ABI {ORDINARY_ABI}")),
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
