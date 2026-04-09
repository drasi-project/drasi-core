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

//! Plugin loader — discovers, validates, and loads cdylib plugins.

use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use indexmap::IndexMap;
use libloading::{Library, Symbol};

use drasi_plugin_sdk::ffi::{
    FfiPluginRegistration, LifecycleCallbackFn, LogCallbackFn, PluginMetadata,
};

use crate::proxies::bootstrap_provider::BootstrapPluginProxy;
use crate::proxies::reaction::ReactionPluginProxy;
use crate::proxies::source::SourcePluginProxy;

/// Configuration for the plugin loader.
#[derive(Debug, Clone)]
pub struct PluginLoaderConfig {
    /// Directory to scan for plugin shared libraries.
    pub plugin_dir: PathBuf,
    /// File glob patterns to match (e.g., `["libdrasi_source_*", "libdrasi_reaction_*"]`).
    pub file_patterns: Vec<String>,
}

/// A loaded plugin with its metadata and factory proxies.
pub struct LoadedPlugin {
    /// Source plugin factories (descriptor proxies).
    pub source_plugins: Vec<SourcePluginProxy>,
    /// Reaction plugin factories (descriptor proxies).
    pub reaction_plugins: Vec<ReactionPluginProxy>,
    /// Bootstrap plugin factories (descriptor proxies).
    pub bootstrap_plugins: Vec<BootstrapPluginProxy>,
    /// Plugin metadata string for diagnostics.
    pub metadata_info: Option<String>,
    /// Path to the loaded plugin file.
    pub file_path: PathBuf,
    /// Keep the library loaded.
    _library: Arc<Library>,
}

impl Drop for LoadedPlugin {
    fn drop(&mut self) {
        // 1. Drop all plugin proxies (calls FFI drop functions on plugin state).
        self.source_plugins.clear();
        self.reaction_plugins.clear();
        self.bootstrap_plugins.clear();

        // 2. Leak the library so dlclose is never called.
        //    Plugin runtime worker threads may still be executing library code;
        //    calling dlclose while worker threads run is UB on macOS.
        //    Bumping the Arc refcount prevents Library::drop → dlclose when
        //    Rust drops the `_library` field after this method returns.
        //
        //    Note: we intentionally do NOT call drasi_plugin_shutdown() because
        //    dlopen returns the same library handle for the same dylib path,
        //    so all LoadedPlugin instances share the same process-global statics.
        //    Calling shutdown() from one instance would null the runtime pointer
        //    for all instances, causing null pointer dereferences.
        std::mem::forget(self._library.clone());
    }
}

/// Known cdylib shared library extensions, in preference order.
const CDYLIB_EXTENSIONS: &[&str] = &[".dylib", ".so", ".dll"];

/// All extensions we might encounter (cdylib + Cargo build artifacts).
const ALL_KNOWN_EXTENSIONS: &[&str] = &[".dylib", ".so", ".dll", ".rlib", ".rmeta", ".d"];

/// Loads cdylib plugins from a directory.
pub struct PluginLoader {
    config: PluginLoaderConfig,
}

impl PluginLoader {
    pub fn new(config: PluginLoaderConfig) -> Self {
        Self { config }
    }

    /// Load all plugins matching the configured patterns.
    ///
    /// Discovers candidate files, groups them by plugin base name, then loads
    /// exactly one cdylib per plugin. Non-cdylib artifacts (.rlib, .d, .rmeta)
    /// are silently ignored. An error is logged if multiple cdylib extensions
    /// exist for the same plugin (ambiguous).
    pub fn load_all(
        &self,
        log_ctx: *mut c_void,
        log_callback: LogCallbackFn,
        lifecycle_ctx: *mut c_void,
        lifecycle_callback: LifecycleCallbackFn,
    ) -> anyhow::Result<Vec<LoadedPlugin>> {
        let mut plugins = Vec::new();
        let plugin_dir = &self.config.plugin_dir;

        if !plugin_dir.exists() {
            log::warn!("Plugin directory does not exist: {}", plugin_dir.display());
            return Ok(plugins);
        }

        // Phase 1: Discover all candidate files and group by plugin name
        let candidates = discover_plugin_candidates(plugin_dir, &self.config.file_patterns);

        // Phase 2: For each plugin name, try loading from cdylib extensions
        for (plugin_name, files) in &candidates {
            let cdylib_files: Vec<&PathBuf> = files
                .iter()
                .filter(|p| {
                    CDYLIB_EXTENSIONS
                        .iter()
                        .any(|ext| p.to_string_lossy().ends_with(ext))
                })
                .collect();

            if cdylib_files.is_empty() {
                log::debug!(
                    "Plugin '{}': no cdylib files found (skipping {} non-cdylib file(s))",
                    plugin_name,
                    files.len()
                );
                continue;
            }

            if cdylib_files.len() > 1 {
                log::error!(
                    "Plugin '{}': found {} cdylib files — ambiguous. \
                     Remove duplicates and keep only one: {:?}",
                    plugin_name,
                    cdylib_files.len(),
                    cdylib_files
                );
                continue;
            }

            // Exactly one cdylib file — load it
            let path = cdylib_files[0];
            match self.load_plugin(
                path,
                log_ctx,
                log_callback,
                lifecycle_ctx,
                lifecycle_callback,
            ) {
                Ok(plugin) => {
                    log::info!(
                        "Loaded plugin: {} ({})",
                        path.display(),
                        plugin.metadata_info.as_deref().unwrap_or("no metadata")
                    );
                    plugins.push(plugin);
                }
                Err(e) => {
                    log::error!("Failed to load plugin {}: {}", path.display(), e);
                }
            }
        }

        Ok(plugins)
    }

    /// Load a single plugin from a path.
    pub fn load_plugin(
        &self,
        path: &Path,
        log_ctx: *mut c_void,
        log_callback: LogCallbackFn,
        lifecycle_ctx: *mut c_void,
        lifecycle_callback: LifecycleCallbackFn,
    ) -> anyhow::Result<LoadedPlugin> {
        load_plugin_from_path(
            path,
            log_ctx,
            log_callback,
            lifecycle_ctx,
            lifecycle_callback,
        )
    }
}

/// Load a single plugin from a shared library path.
///
/// This function:
/// 1. Opens the shared library
/// 2. Resolves and validates `drasi_plugin_metadata()`
/// 3. Calls `drasi_plugin_init()` to get the registration
/// 4. Wires log and lifecycle callbacks
/// 5. Extracts factory vtables into proxy types
pub fn load_plugin_from_path(
    path: &Path,
    log_ctx: *mut c_void,
    log_callback: LogCallbackFn,
    lifecycle_ctx: *mut c_void,
    lifecycle_callback: LifecycleCallbackFn,
) -> anyhow::Result<LoadedPlugin> {
    let lib = Arc::new(unsafe {
        Library::new(path)
            .map_err(|e| anyhow::anyhow!("Failed to load {}: {}", path.display(), e))?
    });

    // Step 1: Read and validate metadata
    let metadata_info = read_plugin_metadata(&lib);
    validate_plugin_metadata(&lib, path)?;

    // Step 2: Call drasi_plugin_init()
    let init_fn: Symbol<unsafe extern "C" fn() -> *mut FfiPluginRegistration> = unsafe {
        lib.get(b"drasi_plugin_init").map_err(|e| {
            anyhow::anyhow!("Missing drasi_plugin_init in {}: {}", path.display(), e)
        })?
    };

    let reg_ptr = unsafe { init_fn() };
    if reg_ptr.is_null() {
        return Err(anyhow::anyhow!(
            "drasi_plugin_init returned null (init panicked?) in {}",
            path.display()
        ));
    }

    let registration = unsafe { Box::from_raw(reg_ptr) };

    // Step 3: Wire callbacks (with host-owned context pointers)
    (registration.set_log_callback)(log_ctx, log_callback);
    (registration.set_lifecycle_callback)(lifecycle_ctx, lifecycle_callback);

    // Step 4: Extract factory vtables into proxies
    // Take ownership of ALL arrays upfront before processing, so if any
    // proxy construction panics, remaining arrays are still dropped correctly.
    let source_vtables =
        if !registration.source_plugins.is_null() && registration.source_plugin_count > 0 {
            Some(unsafe {
                Vec::from_raw_parts(
                    registration.source_plugins,
                    registration.source_plugin_count,
                    registration.source_plugin_count,
                )
            })
        } else {
            None
        };

    let reaction_vtables =
        if !registration.reaction_plugins.is_null() && registration.reaction_plugin_count > 0 {
            Some(unsafe {
                Vec::from_raw_parts(
                    registration.reaction_plugins,
                    registration.reaction_plugin_count,
                    registration.reaction_plugin_count,
                )
            })
        } else {
            None
        };

    let bootstrap_vtables =
        if !registration.bootstrap_plugins.is_null() && registration.bootstrap_plugin_count > 0 {
            Some(unsafe {
                Vec::from_raw_parts(
                    registration.bootstrap_plugins,
                    registration.bootstrap_plugin_count,
                    registration.bootstrap_plugin_count,
                )
            })
        } else {
            None
        };

    // Now safe to forget the registration — we own all arrays
    std::mem::forget(registration);

    // Process vtables into proxies
    let mut source_plugins = Vec::new();
    let mut reaction_plugins = Vec::new();
    let mut bootstrap_plugins = Vec::new();

    for v in source_vtables.into_iter().flatten() {
        source_plugins.push(SourcePluginProxy::new(v, lib.clone()));
    }

    for v in reaction_vtables.into_iter().flatten() {
        reaction_plugins.push(ReactionPluginProxy::new(v, lib.clone()));
    }

    for v in bootstrap_vtables.into_iter().flatten() {
        bootstrap_plugins.push(BootstrapPluginProxy::new(v, lib.clone()));
    }

    Ok(LoadedPlugin {
        source_plugins,
        reaction_plugins,
        bootstrap_plugins,
        metadata_info,
        file_path: path.to_path_buf(),
        _library: lib,
    })
}

/// Read plugin metadata from the shared library.
fn read_plugin_metadata(lib: &Library) -> Option<String> {
    unsafe {
        if let Ok(meta_fn) =
            lib.get::<unsafe extern "C" fn() -> *const PluginMetadata>(b"drasi_plugin_metadata")
        {
            let meta_ptr = meta_fn();
            if !meta_ptr.is_null() {
                let meta = &*meta_ptr;
                let sdk_ver = meta.sdk_version.to_string();
                let core_ver = meta.core_version.to_string();
                let plugin_ver = meta.plugin_version.to_string();
                let target = meta.target_triple.to_string();
                let commit = meta.git_commit.to_string();
                let built = meta.build_timestamp.to_string();
                Some(format!(
                    "sdk={sdk_ver} core={core_ver} plugin={plugin_ver} target={target} commit={commit} built={built}"
                ))
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// Validate plugin metadata against the host SDK version.
///
/// Checks that the plugin's SDK version is compatible with the host.
/// For cdylib plugins, we check major.minor compatibility (patch differences are OK).
fn validate_plugin_metadata(lib: &Library, path: &Path) -> anyhow::Result<()> {
    let meta_fn = unsafe {
        match lib.get::<unsafe extern "C" fn() -> *const PluginMetadata>(b"drasi_plugin_metadata") {
            Ok(f) => f,
            Err(_) => {
                log::warn!(
                    "Plugin '{}' does not export drasi_plugin_metadata — skipping version check",
                    path.display()
                );
                return Ok(());
            }
        }
    };

    let meta_ptr = unsafe { meta_fn() };
    if meta_ptr.is_null() {
        log::warn!(
            "Plugin '{}' returned null metadata — skipping version check",
            path.display()
        );
        return Ok(());
    }

    let meta = unsafe { &*meta_ptr };
    let plugin_sdk_version = unsafe { meta.sdk_version.to_string() };
    let host_sdk_version = drasi_plugin_sdk::ffi::metadata::FFI_SDK_VERSION;

    // Check major.minor compatibility
    let plugin_parts: Vec<&str> = plugin_sdk_version.split('.').collect();
    let host_parts: Vec<&str> = host_sdk_version.split('.').collect();

    let plugin_major_minor = format!(
        "{}.{}",
        plugin_parts.first().unwrap_or(&"0"),
        plugin_parts.get(1).unwrap_or(&"0")
    );
    let host_major_minor = format!(
        "{}.{}",
        host_parts.first().unwrap_or(&"0"),
        host_parts.get(1).unwrap_or(&"0")
    );

    if plugin_major_minor != host_major_minor {
        anyhow::bail!(
            "Plugin '{}' SDK version mismatch: plugin={}, host={}. \
             Major.minor versions must match ({} != {}).",
            path.display(),
            plugin_sdk_version,
            host_sdk_version,
            plugin_major_minor,
            host_major_minor,
        );
    }

    // Check target triple compatibility
    let plugin_target = unsafe { meta.target_triple.to_string() };
    let host_target = drasi_plugin_sdk::ffi::metadata::TARGET_TRIPLE;
    if plugin_target != host_target {
        anyhow::bail!(
            "Plugin '{}' target mismatch: plugin={}, host={}. \
             Plugins must be built for the same target platform.",
            path.display(),
            plugin_target,
            host_target,
        );
    }

    log::debug!(
        "Plugin '{}' version check passed: sdk={} target={}",
        path.display(),
        plugin_sdk_version,
        plugin_target
    );

    Ok(())
}

/// Scan the plugin directory and group files by plugin base name.
///
/// Returns an ordered map of plugin_name → Vec<PathBuf> where plugin_name is
/// the filename with all known extensions stripped (e.g., "libdrasi_source_mock").
fn discover_plugin_candidates(dir: &Path, patterns: &[String]) -> IndexMap<String, Vec<PathBuf>> {
    let mut groups: IndexMap<String, Vec<PathBuf>> = IndexMap::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return groups,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        // Strip any known extension to get the base name
        let base_name = ALL_KNOWN_EXTENSIONS
            .iter()
            .find_map(|ext| file_name.strip_suffix(ext))
            .unwrap_or(&file_name)
            .to_string();

        // Check if the base name matches any of the configured patterns
        let matched = patterns.iter().any(|pattern| {
            let pat = ALL_KNOWN_EXTENSIONS
                .iter()
                .find_map(|ext| pattern.strip_suffix(ext))
                .unwrap_or(pattern);
            matches_glob(pat, &base_name)
        });

        if matched {
            groups.entry(base_name).or_default().push(path);
        }
    }

    groups
}

/// Simple glob matching: supports trailing `*` and middle `*`.
fn matches_glob(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else if let Some((prefix, suffix)) = pattern.split_once('*') {
        name.starts_with(prefix) && name.ends_with(suffix)
    } else {
        name == pattern
    }
}

/// Helper to get the platform-specific plugin file path.
pub fn plugin_path(dir: &Path, name: &str) -> PathBuf {
    if cfg!(target_os = "macos") {
        dir.join(format!("lib{name}.dylib"))
    } else if cfg!(target_os = "windows") {
        dir.join(format!("{name}.dll"))
    } else {
        dir.join(format!("lib{name}.so"))
    }
}

// ── Shared naming / discovery helpers ──

/// Default file patterns for discovering Drasi cdylib plugins.
/// Includes both Unix (`lib` prefix) and Windows (no prefix) naming conventions.
pub const DEFAULT_PLUGIN_FILE_PATTERNS: &[&str] = &[
    "libdrasi_source_*",
    "libdrasi_reaction_*",
    "libdrasi_bootstrap_*",
    "drasi_source_*",
    "drasi_reaction_*",
    "drasi_bootstrap_*",
];

/// Known shared library extensions for cdylib plugins.
pub const PLUGIN_BINARY_EXTENSIONS: &[&str] = CDYLIB_EXTENSIONS;

/// Check whether a filename looks like a Drasi plugin binary.
pub fn is_plugin_binary(name: &str) -> bool {
    CDYLIB_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
}

/// Extract a `"type/kind"` string from a Drasi plugin filename.
///
/// For example:
/// - `"libdrasi_source_postgres.so"` → `Some("source/postgres")`
/// - `"drasi_reaction_log.dll"` → `Some("reaction/log")`
/// - `"not_a_plugin.txt"` → `None`
///
/// Underscores in the kind portion are converted to hyphens.
pub fn plugin_kind_from_filename(filename: &str) -> Option<String> {
    let stem = if let Some(stem) = filename.strip_suffix(".so") {
        stem.strip_prefix("lib")?
    } else if let Some(stem) = filename.strip_suffix(".dll") {
        stem
    } else if let Some(stem) = filename.strip_suffix(".dylib") {
        stem.strip_prefix("lib")?
    } else {
        return None;
    };

    let stem = stem.strip_prefix("drasi_")?;
    let mut parts = stem.splitn(2, '_');
    let ptype = parts.next()?;
    let kind = parts.next()?.replace('_', "-");
    Some(format!("{ptype}/{kind}"))
}

/// Summary of a plugin's metadata read without full initialization.
///
/// Obtained by calling only `drasi_plugin_metadata()` — no tokio runtime
/// is started and no `drasi_plugin_init()` is called.
#[derive(Debug, Clone)]
pub struct PluginMetadataSummary {
    pub plugin_id: String,
    pub version: String,
    pub sdk_version: String,
    pub core_version: String,
    pub target_triple: String,
    pub git_commit: String,
    pub build_timestamp: String,
    pub file_path: PathBuf,
}

/// Read a plugin's metadata without fully initializing it.
///
/// This calls only `drasi_plugin_metadata()` via `dlopen` + symbol lookup.
/// No tokio runtime is created and no `drasi_plugin_init()` is called, making
/// this safe and fast for scanning/inspection flows.
///
/// Returns `None` if the library cannot be loaded or does not export metadata.
pub fn scan_plugin_metadata(path: &Path) -> Option<PluginMetadataSummary> {
    let lib = unsafe { Library::new(path).ok()? };
    let meta_fn = unsafe {
        lib.get::<unsafe extern "C" fn() -> *const PluginMetadata>(b"drasi_plugin_metadata")
            .ok()?
    };
    let meta_ptr = unsafe { meta_fn() };
    if meta_ptr.is_null() {
        return None;
    }
    let meta = unsafe { &*meta_ptr };
    let sdk_version = unsafe { meta.sdk_version.to_string() };
    let core_version = unsafe { meta.core_version.to_string() };
    let plugin_version = unsafe { meta.plugin_version.to_string() };
    let target_triple = unsafe { meta.target_triple.to_string() };
    let git_commit = unsafe { meta.git_commit.to_string() };
    let build_timestamp = unsafe { meta.build_timestamp.to_string() };

    // Derive a plugin_id from the filename using the naming convention.
    let plugin_id = path
        .file_name()
        .and_then(|f| f.to_str())
        .and_then(plugin_kind_from_filename)
        .unwrap_or_default();

    // Close the library — we only needed the metadata strings (already copied).
    // Unlike LoadedPlugin (which keeps vtable pointers alive), scan_plugin_metadata
    // has no dangling references after the strings are copied above.
    drop(lib);

    Some(PluginMetadataSummary {
        plugin_id,
        version: plugin_version,
        sdk_version,
        core_version,
        target_triple,
        git_commit,
        build_timestamp,
        file_path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Create a temp dir with the given filenames (empty files).
    fn setup_temp_dir(files: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for f in files {
            fs::write(dir.path().join(f), b"").unwrap();
        }
        dir
    }

    // ── matches_glob tests ──

    #[test]
    fn test_matches_glob_prefix_wildcard() {
        assert!(matches_glob("libdrasi_source_*", "libdrasi_source_mock"));
        assert!(matches_glob("libdrasi_source_*", "libdrasi_source_http"));
        assert!(!matches_glob("libdrasi_source_*", "libdrasi_reaction_log"));
    }

    #[test]
    fn test_matches_glob_exact() {
        assert!(matches_glob("libdrasi_source_mock", "libdrasi_source_mock"));
        assert!(!matches_glob(
            "libdrasi_source_mock",
            "libdrasi_source_http"
        ));
    }

    #[test]
    fn test_matches_glob_middle_wildcard() {
        assert!(matches_glob("lib*mock", "libdrasi_source_mock"));
        assert!(!matches_glob("lib*mock", "libdrasi_source_http"));
    }

    // ── discover_plugin_candidates tests ──

    #[test]
    fn test_discover_groups_by_base_name() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_mock.dylib",
            "libdrasi_source_mock.rlib",
            "libdrasi_source_mock.d",
        ]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("libdrasi_source_mock"));
        assert_eq!(groups["libdrasi_source_mock"].len(), 3);
    }

    #[test]
    fn test_discover_ignores_non_matching_files() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_mock.dylib",
            "unrelated_file.txt",
            "libfoo.so",
        ]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("libdrasi_source_mock"));
    }

    #[test]
    fn test_discover_multiple_plugins() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_mock.dylib",
            "libdrasi_source_mock.rlib",
            "libdrasi_source_http.so",
            "libdrasi_source_http.rmeta",
        ]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        assert_eq!(groups.len(), 2);
        assert!(groups.contains_key("libdrasi_source_mock"));
        assert!(groups.contains_key("libdrasi_source_http"));
    }

    #[test]
    fn test_discover_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        assert!(groups.is_empty());
    }

    #[test]
    fn test_discover_nonexistent_dir() {
        let groups = discover_plugin_candidates(Path::new("/nonexistent"), &["libdrasi_*".into()]);
        assert!(groups.is_empty());
    }

    // ── cdylib filtering tests ──

    #[test]
    fn test_cdylib_only_filtering() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_mock.dylib",
            "libdrasi_source_mock.rlib",
            "libdrasi_source_mock.d",
        ]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);
        let files = &groups["libdrasi_source_mock"];

        let cdylib_files: Vec<&PathBuf> = files
            .iter()
            .filter(|p| {
                CDYLIB_EXTENSIONS
                    .iter()
                    .any(|ext| p.to_string_lossy().ends_with(ext))
            })
            .collect();

        assert_eq!(cdylib_files.len(), 1);
        assert!(cdylib_files[0]
            .to_string_lossy()
            .ends_with("libdrasi_source_mock.dylib"));
    }

    #[test]
    fn test_ambiguous_cdylib_detected() {
        let dir = setup_temp_dir(&["libdrasi_source_mock.dylib", "libdrasi_source_mock.so"]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);
        let files = &groups["libdrasi_source_mock"];

        let cdylib_files: Vec<&PathBuf> = files
            .iter()
            .filter(|p| {
                CDYLIB_EXTENSIONS
                    .iter()
                    .any(|ext| p.to_string_lossy().ends_with(ext))
            })
            .collect();

        assert_eq!(
            cdylib_files.len(),
            2,
            "Should detect 2 ambiguous cdylib files"
        );
    }

    #[test]
    fn test_no_cdylib_skips_silently() {
        let dir = setup_temp_dir(&["libdrasi_source_mock.rlib", "libdrasi_source_mock.d"]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);
        let files = &groups["libdrasi_source_mock"];

        let cdylib_files: Vec<&PathBuf> = files
            .iter()
            .filter(|p| {
                CDYLIB_EXTENSIONS
                    .iter()
                    .any(|ext| p.to_string_lossy().ends_with(ext))
            })
            .collect();

        assert!(
            cdylib_files.is_empty(),
            "Should find no cdylib files when only .rlib and .d exist"
        );
    }

    #[test]
    fn test_discover_with_pattern_including_extension() {
        let dir = setup_temp_dir(&["libdrasi_source_mock.dylib", "libdrasi_source_mock.rlib"]);
        // Pattern includes an extension — should still match base name
        let patterns = vec!["libdrasi_source_*.dylib".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("libdrasi_source_mock"));
        assert_eq!(groups["libdrasi_source_mock"].len(), 2);
    }

    #[test]
    fn test_discover_multiple_patterns() {
        let dir = setup_temp_dir(&[
            "libdrasi_source_mock.dylib",
            "libdrasi_reaction_log.so",
            "libdrasi_bootstrap_mock.dylib",
        ]);
        let patterns = vec![
            "libdrasi_source_*".to_string(),
            "libdrasi_reaction_*".to_string(),
        ];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        assert_eq!(groups.len(), 2);
        assert!(groups.contains_key("libdrasi_source_mock"));
        assert!(groups.contains_key("libdrasi_reaction_log"));
        assert!(!groups.contains_key("libdrasi_bootstrap_mock"));
    }

    #[test]
    fn test_file_without_known_extension_matched_by_base() {
        let dir = setup_temp_dir(&["libdrasi_source_mock"]);
        let patterns = vec!["libdrasi_source_*".to_string()];
        let groups = discover_plugin_candidates(dir.path(), &patterns);

        // File has no known extension, so base_name == filename
        assert_eq!(groups.len(), 1);
        assert!(groups.contains_key("libdrasi_source_mock"));
    }

    // ── Naming / discovery helper tests ──

    #[test]
    fn test_plugin_kind_from_filename_unix() {
        assert_eq!(
            plugin_kind_from_filename("libdrasi_source_postgres.so"),
            Some("source/postgres".to_string())
        );
        assert_eq!(
            plugin_kind_from_filename("libdrasi_reaction_log.dylib"),
            Some("reaction/log".to_string())
        );
        assert_eq!(
            plugin_kind_from_filename("libdrasi_bootstrap_postgres.so"),
            Some("bootstrap/postgres".to_string())
        );
    }

    #[test]
    fn test_plugin_kind_from_filename_windows() {
        assert_eq!(
            plugin_kind_from_filename("drasi_source_postgres.dll"),
            Some("source/postgres".to_string())
        );
    }

    #[test]
    fn test_plugin_kind_from_filename_underscore_to_hyphen() {
        assert_eq!(
            plugin_kind_from_filename("libdrasi_source_postgres_replication.so"),
            Some("source/postgres-replication".to_string())
        );
    }

    #[test]
    fn test_plugin_kind_from_filename_not_a_plugin() {
        assert_eq!(plugin_kind_from_filename("random_lib.so"), None);
        assert_eq!(plugin_kind_from_filename("not_a_plugin.txt"), None);
    }

    #[test]
    fn test_is_plugin_binary() {
        assert!(is_plugin_binary("libdrasi_source_mock.so"));
        assert!(is_plugin_binary("drasi_reaction_log.dll"));
        assert!(is_plugin_binary("libdrasi_bootstrap_postgres.dylib"));
        assert!(!is_plugin_binary("plugin.rlib"));
        assert!(!is_plugin_binary("readme.md"));
    }

    #[test]
    #[allow(clippy::const_is_empty)]
    fn test_default_patterns_not_empty() {
        assert!(!DEFAULT_PLUGIN_FILE_PATTERNS.is_empty());
    }
}
