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

//! Plugin lockfile management.
//!
//! The lockfile (`plugins.lock`) records the exact resolved versions, digests,
//! and filenames for each plugin dependency. This enables reproducible installs
//! and the `--locked` flag to enforce exact versions.

use anyhow::{bail, Context, Result};
use log::{debug, info};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const LOCKFILE_NAME: &str = "plugins.lock";
const LOCKFILE_VERSION: u32 = 1;

/// The top-level lockfile structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginLockfile {
    /// Lockfile format version.
    pub version: u32,
    /// Locked plugin entries keyed by the original reference string.
    #[serde(default)]
    pub plugins: BTreeMap<String, LockedPlugin>,
}

/// A single locked plugin entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockedPlugin {
    /// Fully qualified OCI reference with digest (e.g., ghcr.io/drasi-project/source/postgres@sha256:abc...).
    pub reference: String,
    /// Resolved version tag.
    pub version: String,
    /// Content digest (sha256:...).
    pub digest: String,
    /// SDK version of the plugin.
    pub sdk_version: String,
    /// Core version of the plugin.
    pub core_version: String,
    /// Lib version of the plugin.
    pub lib_version: String,
    /// OCI platform string (e.g., linux/amd64).
    pub platform: String,
    /// Expected binary filename (e.g., libdrasi_source_postgres.so).
    pub filename: String,
    /// SHA-256 hash of the plugin binary file (hex-encoded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<String>,
    /// Git commit SHA the plugin was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Build timestamp in RFC 3339 format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<String>,
    /// Cosign signature verification result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<PluginSignatureInfo>,
}

/// Cached cosign signature verification result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginSignatureInfo {
    /// Whether the signature was successfully verified.
    pub verified: bool,
    /// OIDC issuer of the signing certificate.
    pub issuer: String,
    /// Subject (SAN) from the signing certificate.
    pub subject: String,
}

impl PluginLockfile {
    /// Create a new empty lockfile.
    pub fn new() -> Self {
        Self {
            version: LOCKFILE_VERSION,
            plugins: BTreeMap::new(),
        }
    }

    /// Read a lockfile from disk. Returns None if the file doesn't exist.
    pub fn read(dir: &Path) -> Result<Option<Self>> {
        let path = dir.join(LOCKFILE_NAME);
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let lockfile: Self = toml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        if lockfile.version != LOCKFILE_VERSION {
            bail!(
                "unsupported lockfile version {} (expected {})",
                lockfile.version,
                LOCKFILE_VERSION
            );
        }

        debug!("Read lockfile with {} entries", lockfile.plugins.len());
        Ok(Some(lockfile))
    }

    /// Write the lockfile to disk.
    pub fn write(&self, dir: &Path) -> Result<()> {
        let path = dir.join(LOCKFILE_NAME);
        let content = toml::to_string_pretty(self).context("failed to serialize lockfile")?;

        // Atomic write: temp file + rename
        let tmp_path = dir.join(".plugins.lock.tmp");
        std::fs::write(&tmp_path, &content)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("failed to rename lockfile to {}", path.display()))?;

        info!("Updated {}", path.display());
        Ok(())
    }

    /// Get a locked entry for a plugin reference.
    pub fn get(&self, reference: &str) -> Option<&LockedPlugin> {
        self.plugins.get(reference)
    }

    /// Insert or update a locked entry.
    ///
    /// If another entry with a different key but the same filename already exists,
    /// it is removed first. This prevents duplicate entries when the same plugin
    /// is installed from different sources (e.g., first from `file://`, then from OCI).
    pub fn insert(&mut self, reference: String, entry: LockedPlugin) {
        // Remove any existing entry that maps to the same filename under a different key
        let conflicting_key = self
            .plugins
            .iter()
            .find(|(k, v)| v.filename == entry.filename && *k != &reference)
            .map(|(k, _)| k.clone());

        if let Some(old_key) = conflicting_key {
            log::debug!(
                "Replacing lockfile entry '{}' (same filename '{}')",
                old_key,
                entry.filename
            );
            self.plugins.remove(&old_key);
        }

        self.plugins.insert(reference, entry);
    }

    /// Remove a locked entry.
    pub fn remove(&mut self, reference: &str) -> Option<LockedPlugin> {
        self.plugins.remove(reference)
    }

    /// Iterate over all locked plugin entries.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &LockedPlugin)> {
        self.plugins.iter()
    }

    /// Get all reference keys.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.plugins.keys()
    }

    /// Check if the lockfile has no plugins.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Verify the integrity of all plugin files in the given directory.
    ///
    /// Returns the set of filenames that passed integrity verification.
    /// Files with no recorded hash are considered unverified and excluded.
    pub fn verify_file_integrity(
        &self,
        plugins_dir: &Path,
    ) -> std::collections::HashMap<String, FileIntegrityStatus> {
        let mut results = std::collections::HashMap::new();
        for entry in self.plugins.values() {
            let file_path = plugins_dir.join(&entry.filename);
            let status = if !file_path.exists() {
                FileIntegrityStatus::Missing
            } else if let Some(expected_hash) = &entry.file_hash {
                match compute_file_hash(&file_path) {
                    Ok(actual_hash) if actual_hash == *expected_hash => FileIntegrityStatus::Ok,
                    Ok(actual_hash) => FileIntegrityStatus::Tampered {
                        expected: expected_hash.clone(),
                        actual: actual_hash,
                    },
                    Err(e) => FileIntegrityStatus::Error(format!("{e}")),
                }
            } else {
                FileIntegrityStatus::NoHash
            };
            results.insert(entry.filename.clone(), status);
        }
        results
    }
}

/// Result of a file integrity check.
#[derive(Debug, Clone, PartialEq)]
pub enum FileIntegrityStatus {
    /// File hash matches the lockfile.
    Ok,
    /// File hash does not match — the binary has been modified.
    Tampered { expected: String, actual: String },
    /// Plugin file is missing from disk.
    Missing,
    /// No hash recorded in the lockfile (legacy entry).
    NoHash,
    /// Error computing hash.
    Error(String),
}

/// Compute the SHA-256 hash of a file, returned as a hex string.
pub fn compute_file_hash(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let data = std::fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let hash = Sha256::digest(&data);
    Ok(format!("{hash:x}"))
}

impl Default for PluginLockfile {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_entry() -> LockedPlugin {
        LockedPlugin {
            reference: "ghcr.io/drasi-project/source/postgres@sha256:abc123".to_string(),
            version: "0.1.8".to_string(),
            digest: "sha256:abc123".to_string(),
            sdk_version: "0.3.1".to_string(),
            core_version: "0.3.3".to_string(),
            lib_version: "0.3.8".to_string(),
            platform: "linux/amd64".to_string(),
            filename: "libdrasi_source_postgres.so".to_string(),
            file_hash: None,
            git_commit: None,
            build_timestamp: None,
            signature: None,
        }
    }

    #[test]
    fn test_lockfile_roundtrip() {
        let dir = TempDir::new().unwrap();
        let mut lockfile = PluginLockfile::new();
        lockfile.insert("source/postgres".to_string(), sample_entry());
        lockfile.insert(
            "reaction/log".to_string(),
            LockedPlugin {
                reference: "ghcr.io/drasi-project/reaction/log@sha256:def456".to_string(),
                version: "0.1.7".to_string(),
                digest: "sha256:def456".to_string(),
                sdk_version: "0.3.1".to_string(),
                core_version: "0.3.3".to_string(),
                lib_version: "0.3.8".to_string(),
                platform: "linux/amd64".to_string(),
                filename: "libdrasi_reaction_log.so".to_string(),
                file_hash: None,
                git_commit: None,
                build_timestamp: None,
                signature: None,
            },
        );

        lockfile.write(dir.path()).unwrap();

        let loaded = PluginLockfile::read(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.plugins.len(), 2);
        assert_eq!(loaded.get("source/postgres").unwrap(), &sample_entry());
    }

    #[test]
    fn test_lockfile_missing_returns_none() {
        let dir = TempDir::new().unwrap();
        assert!(PluginLockfile::read(dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_lockfile_toml_format() {
        let mut lockfile = PluginLockfile::new();
        lockfile.insert("source/postgres".to_string(), sample_entry());

        let content = toml::to_string_pretty(&lockfile).unwrap();
        assert!(content.contains("version = 1"));
        assert!(content.contains("[plugins.\"source/postgres\"]"));
        assert!(content.contains("sha256:abc123"));
    }

    #[test]
    fn test_lockfile_remove() {
        let mut lockfile = PluginLockfile::new();
        lockfile.insert("source/postgres".to_string(), sample_entry());
        assert!(lockfile.get("source/postgres").is_some());

        lockfile.remove("source/postgres");
        assert!(lockfile.get("source/postgres").is_none());
    }

    #[test]
    fn test_lockfile_deduplicates_by_filename() {
        let mut lockfile = PluginLockfile::new();

        // Install from file
        lockfile.insert(
            "file:///path/to/libdrasi_source_postgres.so".to_string(),
            LockedPlugin {
                reference: "file:///path/to/libdrasi_source_postgres.so".to_string(),
                version: "0.1.8".to_string(),
                digest: String::new(),
                sdk_version: "0.3.1".to_string(),
                core_version: "0.3.3".to_string(),
                lib_version: "0.3.8".to_string(),
                platform: "x86_64-unknown-linux-gnu".to_string(),
                filename: "libdrasi_source_postgres.so".to_string(),
                file_hash: None,
                git_commit: None,
                build_timestamp: None,
                signature: None,
            },
        );
        assert_eq!(lockfile.plugins.len(), 1);

        // Install same plugin from OCI — should replace the file entry
        lockfile.insert("source/postgres".to_string(), sample_entry());
        assert_eq!(lockfile.plugins.len(), 1);
        assert!(lockfile.get("source/postgres").is_some());
        assert!(lockfile
            .get("file:///path/to/libdrasi_source_postgres.so")
            .is_none());
    }

    #[test]
    fn test_compute_file_hash() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("test_plugin.so");
        std::fs::write(&file_path, b"hello world").unwrap();

        let hash = compute_file_hash(&file_path).unwrap();

        // SHA-256 of "hello world"
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_compute_file_hash_missing_file() {
        let dir = TempDir::new().unwrap();
        let result = compute_file_hash(&dir.path().join("nonexistent.so"));
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_file_integrity_ok() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("libdrasi_source_postgres.so");
        std::fs::write(&file_path, b"plugin binary content").unwrap();

        let hash = compute_file_hash(&file_path).unwrap();

        let mut lockfile = PluginLockfile::new();
        let mut entry = sample_entry();
        entry.file_hash = Some(hash);
        lockfile.insert("source/postgres".to_string(), entry);

        let results = lockfile.verify_file_integrity(dir.path());
        assert_eq!(
            results.get("libdrasi_source_postgres.so"),
            Some(&FileIntegrityStatus::Ok)
        );
    }

    #[test]
    fn test_verify_file_integrity_tampered() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("libdrasi_source_postgres.so");
        std::fs::write(&file_path, b"original content").unwrap();

        let original_hash = compute_file_hash(&file_path).unwrap();

        let mut lockfile = PluginLockfile::new();
        let mut entry = sample_entry();
        entry.file_hash = Some(original_hash.clone());
        lockfile.insert("source/postgres".to_string(), entry);

        // Tamper with the file
        std::fs::write(&file_path, b"tampered content").unwrap();

        let results = lockfile.verify_file_integrity(dir.path());
        match results.get("libdrasi_source_postgres.so") {
            Some(FileIntegrityStatus::Tampered { expected, actual }) => {
                assert_eq!(expected, &original_hash);
                assert_ne!(expected, actual);
            }
            other => panic!("Expected Tampered, got {other:?}"),
        }
    }

    #[test]
    fn test_verify_file_integrity_missing() {
        let dir = TempDir::new().unwrap();

        let mut lockfile = PluginLockfile::new();
        let mut entry = sample_entry();
        entry.file_hash = Some("abc123".to_string());
        lockfile.insert("source/postgres".to_string(), entry);

        let results = lockfile.verify_file_integrity(dir.path());
        assert_eq!(
            results.get("libdrasi_source_postgres.so"),
            Some(&FileIntegrityStatus::Missing)
        );
    }

    #[test]
    fn test_verify_file_integrity_no_hash() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("libdrasi_source_postgres.so");
        std::fs::write(&file_path, b"plugin content").unwrap();

        let mut lockfile = PluginLockfile::new();
        lockfile.insert("source/postgres".to_string(), sample_entry());

        let results = lockfile.verify_file_integrity(dir.path());
        assert_eq!(
            results.get("libdrasi_source_postgres.so"),
            Some(&FileIntegrityStatus::NoHash)
        );
    }

    #[test]
    fn test_file_hash_roundtrip_in_lockfile() {
        let dir = TempDir::new().unwrap();

        let mut lockfile = PluginLockfile::new();
        let mut entry = sample_entry();
        entry.file_hash = Some("abcdef1234567890".to_string());
        lockfile.insert("source/postgres".to_string(), entry);

        lockfile.write(dir.path()).unwrap();

        let loaded = PluginLockfile::read(dir.path()).unwrap().unwrap();
        let loaded_entry = loaded.get("source/postgres").unwrap();
        assert_eq!(loaded_entry.file_hash.as_deref(), Some("abcdef1234567890"));
    }

    #[test]
    fn test_file_hash_none_not_serialized() {
        let mut lockfile = PluginLockfile::new();
        lockfile.insert("source/postgres".to_string(), sample_entry());

        let content = toml::to_string_pretty(&lockfile).unwrap();
        assert!(!content.contains("file_hash"));
    }

    #[test]
    fn test_file_hash_some_is_serialized() {
        let mut lockfile = PluginLockfile::new();
        let mut entry = sample_entry();
        entry.file_hash = Some("deadbeef".to_string());
        lockfile.insert("source/postgres".to_string(), entry);

        let content = toml::to_string_pretty(&lockfile).unwrap();
        assert!(content.contains("file_hash = \"deadbeef\""));
    }
}
