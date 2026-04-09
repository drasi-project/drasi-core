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

//! Local directory plugin registry.
//!
//! Scans a local directory for plugin binaries, providing search/resolve/install
//! operations analogous to an OCI registry client.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::loader::{is_plugin_binary, plugin_kind_from_filename, scan_plugin_metadata};

/// A plugin search result from a local directory.
#[derive(Debug, Clone)]
pub struct LocalPluginInfo {
    /// Plugin reference (e.g., "source/postgres").
    pub reference: String,
    /// Filename of the binary (e.g., "libdrasi_source_postgres.dylib").
    pub filename: String,
    /// Plugin version from embedded metadata (empty if metadata unavailable).
    pub version: String,
    /// SDK version from embedded metadata (empty if metadata unavailable).
    pub sdk_version: String,
    /// Full path to the plugin file.
    pub file_path: PathBuf,
}

/// Scans a local directory for plugin binaries, providing search/resolve/install
/// operations analogous to an OCI registry client.
pub struct LocalDirRegistry {
    dir: PathBuf,
}

impl LocalDirRegistry {
    /// Create a new `LocalDirRegistry` pointing at the given directory.
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// Search for plugins matching a pattern (e.g., "source/*", "*", "reaction/log").
    ///
    /// The pattern is matched against the derived plugin reference (e.g., "source/postgres").
    /// A `*` wildcard matches any character sequence (including `/` path separators).
    pub fn search(&self, pattern: &str) -> Result<Vec<LocalPluginInfo>> {
        if !self.dir.exists() {
            bail!(
                "Local plugin directory does not exist: {}",
                self.dir.display()
            );
        }
        if !self.dir.is_dir() {
            bail!(
                "Local plugin path is not a directory: {}",
                self.dir.display()
            );
        }

        let entries = std::fs::read_dir(&self.dir)
            .with_context(|| format!("failed to read directory: {}", self.dir.display()))?;

        let mut results = Vec::new();

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let filename = entry.file_name().to_string_lossy().to_string();

            if !is_plugin_binary(&filename) {
                continue;
            }

            let reference = match plugin_kind_from_filename(&filename) {
                Some(r) => r,
                None => continue,
            };

            if !pattern_matches(pattern, &reference) {
                continue;
            }

            // Try to read embedded metadata for version info
            let (version, sdk_version) = match scan_plugin_metadata(&path) {
                Some(meta) => (meta.version, meta.sdk_version),
                None => (String::new(), String::new()),
            };

            results.push(LocalPluginInfo {
                reference,
                filename,
                version,
                sdk_version,
                file_path: path,
            });
        }

        results.sort_by(|a, b| a.reference.cmp(&b.reference));
        Ok(results)
    }

    /// Resolve a reference (e.g., "source/postgres") to a specific local file.
    pub fn resolve(&self, reference: &str) -> Result<LocalPluginInfo> {
        let results = self.search(reference)?;

        results
            .into_iter()
            .find(|r| r.reference == reference)
            .with_context(|| {
                format!(
                    "plugin '{}' not found in local directory: {}",
                    reference,
                    self.dir.display()
                )
            })
    }

    /// Copy a resolved plugin to the destination directory.
    ///
    /// Returns the path to the copied file.
    pub fn install(&self, info: &LocalPluginInfo, dest_dir: &Path) -> Result<PathBuf> {
        let src = &info.file_path;
        let dest = dest_dir.join(&info.filename);
        std::fs::create_dir_all(dest_dir)
            .with_context(|| format!("failed to create directory: {}", dest_dir.display()))?;
        std::fs::copy(src, &dest)
            .with_context(|| format!("failed to copy {} → {}", src.display(), dest.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
        }

        Ok(dest)
    }
}

/// Simple wildcard pattern matching for plugin references.
///
/// Supports `*` as a wildcard matching any characters (including `/`).
/// A bare `*` matches everything.
fn pattern_matches(pattern: &str, reference: &str) -> bool {
    if pattern == "*" {
        return true;
    }

    let p = pattern.as_bytes();
    let t = reference.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star_pi, mut star_ti) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_pi = Some(pi);
            pi += 1;
            star_ti = ti;
        } else if let Some(sp) = star_pi {
            pi = sp + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_temp_dir(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            fs::write(dir.path().join(f), b"fake-plugin-binary").unwrap();
        }
        dir
    }

    // ── pattern_matches tests ──

    #[test]
    fn test_pattern_wildcard_all() {
        assert!(pattern_matches("*", "source/postgres"));
        assert!(pattern_matches("*", "reaction/log"));
    }

    #[test]
    fn test_pattern_type_wildcard() {
        assert!(pattern_matches("source/*", "source/postgres"));
        assert!(pattern_matches("source/*", "source/mock"));
        assert!(!pattern_matches("source/*", "reaction/log"));
    }

    #[test]
    fn test_pattern_exact() {
        assert!(pattern_matches("source/postgres", "source/postgres"));
        assert!(!pattern_matches("source/postgres", "source/mock"));
    }

    #[test]
    fn test_pattern_kind_wildcard() {
        assert!(pattern_matches("*/postgres", "source/postgres"));
        assert!(!pattern_matches("*/postgres", "source/mock"));
    }

    // ── LocalDirRegistry tests ──

    #[test]
    fn test_search_finds_plugins() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_postgres.dylib",
            "libdrasi_source_mock.dylib",
            "libdrasi_reaction_log.dylib",
            "readme.txt",
        ]);

        let registry = LocalDirRegistry::new(dir.path());
        let results = registry.search("*").unwrap();

        assert_eq!(results.len(), 3);
        let refs: Vec<&str> = results.iter().map(|r| r.reference.as_str()).collect();
        assert!(refs.contains(&"source/postgres"));
        assert!(refs.contains(&"source/mock"));
        assert!(refs.contains(&"reaction/log"));
    }

    #[test]
    fn test_search_filters_by_type() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_postgres.dylib",
            "libdrasi_source_mock.dylib",
            "libdrasi_reaction_log.dylib",
        ]);

        let registry = LocalDirRegistry::new(dir.path());
        let results = registry.search("source/*").unwrap();

        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.reference.starts_with("source/")));
    }

    #[test]
    fn test_search_nonexistent_dir() {
        let registry = LocalDirRegistry::new(Path::new("/nonexistent/path"));
        let result = registry.search("*");
        assert!(result.is_err());
    }

    #[test]
    fn test_search_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let registry = LocalDirRegistry::new(dir.path());
        let results = registry.search("*").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_resolve_found() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_postgres.dylib",
            "libdrasi_source_mock.dylib",
        ]);

        let registry = LocalDirRegistry::new(dir.path());
        let info = registry.resolve("source/postgres").unwrap();
        assert_eq!(info.reference, "source/postgres");
        assert_eq!(info.filename, "libdrasi_source_postgres.dylib");
    }

    #[test]
    fn test_resolve_not_found() {
        let dir = setup_temp_dir(&["libdrasi_source_mock.dylib"]);

        let registry = LocalDirRegistry::new(dir.path());
        let result = registry.resolve("source/postgres");
        assert!(result.is_err());
    }

    #[test]
    fn test_install_copies_file() {
        let src_dir = setup_temp_dir(&["libdrasi_source_mock.dylib"]);
        let dest_dir = tempfile::tempdir().unwrap();

        let registry = LocalDirRegistry::new(src_dir.path());
        let info = registry.resolve("source/mock").unwrap();
        let dest = registry.install(&info, dest_dir.path()).unwrap();

        assert!(dest.exists());
        assert_eq!(dest.file_name().unwrap(), "libdrasi_source_mock.dylib");

        let content = fs::read(&dest).unwrap();
        assert_eq!(content, b"fake-plugin-binary");
    }

    #[cfg(unix)]
    #[test]
    fn test_install_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let src_dir = setup_temp_dir(&["libdrasi_source_mock.dylib"]);
        let dest_dir = tempfile::tempdir().unwrap();

        let registry = LocalDirRegistry::new(src_dir.path());
        let info = registry.resolve("source/mock").unwrap();
        let dest = registry.install(&info, dest_dir.path()).unwrap();

        let mode = fs::metadata(&dest).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    #[test]
    fn test_search_so_extension() {
        let dir = setup_temp_dir(&["libdrasi_source_postgres.so", "libdrasi_reaction_log.so"]);

        let registry = LocalDirRegistry::new(dir.path());
        let results = registry.search("*").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_dll_extension() {
        let dir = setup_temp_dir(&["drasi_source_postgres.dll", "drasi_reaction_log.dll"]);

        let registry = LocalDirRegistry::new(dir.path());
        let results = registry.search("*").unwrap();
        assert_eq!(results.len(), 2);
    }
}
