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

use libloading::{Library, Symbol};

use drasi_plugin_sdk::ffi::{
    FfiPluginRegistration, LifecycleCallbackFn, LogCallbackFn, PluginMetadata,
};

use crate::proxies::bootstrap_provider::BootstrapPluginProxy;
use crate::proxies::identity_provider::IdentityProviderPluginProxy;
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
    /// Identity provider plugin factories (descriptor proxies).
    pub identity_provider_plugins: Vec<IdentityProviderPluginProxy>,
    /// Plugin metadata string for diagnostics.
    pub metadata_info: Option<String>,
    /// Keep the library loaded.
    _library: Arc<Library>,
}

/// Loads cdylib plugins from a directory.
pub struct PluginLoader {
    config: PluginLoaderConfig,
}

impl PluginLoader {
    pub fn new(config: PluginLoaderConfig) -> Self {
        Self { config }
    }

    /// Load all plugins matching the configured patterns.
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

        for pattern in &self.config.file_patterns {
            let glob_pattern = plugin_dir.join(pattern);
            let glob_str = glob_pattern.to_string_lossy();

            // Use simple directory iteration + pattern matching
            if let Ok(entries) = std::fs::read_dir(plugin_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let file_name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    if matches_pattern(&file_name, pattern) {
                        match self.load_plugin(
                            &path,
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
                }
            } else {
                log::warn!("Cannot read plugin directory for pattern: {glob_str}");
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

    // NOTE: We intentionally do not read `identity_provider_plugins` /
    // `identity_provider_plugin_count` from `FfiPluginRegistration` here.
    // Those fields were added in a later SDK version, and older plugins built
    // against the previous ABI may provide a smaller `FfiPluginRegistration`
    // allocation. Accessing the new fields in that case would read beyond the
    // end of the struct, causing undefined behavior. Until ABI/SDK versioning
    // guarantees are tightened, we treat identity provider plugins as absent.
    let identity_provider_vtables: Option<
        Vec<drasi_plugin_sdk::ffi::IdentityProviderPluginVtable>,
    > = None;

    // Now safe to forget the registration — we own all arrays
    std::mem::forget(registration);

    // Process vtables into proxies
    let mut source_plugins = Vec::new();
    let mut reaction_plugins = Vec::new();
    let mut bootstrap_plugins = Vec::new();
    let mut identity_provider_plugins = Vec::new();

    for v in source_vtables.into_iter().flatten() {
        source_plugins.push(SourcePluginProxy::new(v, lib.clone()));
    }

    for v in reaction_vtables.into_iter().flatten() {
        reaction_plugins.push(ReactionPluginProxy::new(v, lib.clone()));
    }

    for v in bootstrap_vtables.into_iter().flatten() {
        bootstrap_plugins.push(BootstrapPluginProxy::new(v, lib.clone()));
    }

    for v in identity_provider_vtables.into_iter().flatten() {
        identity_provider_plugins.push(IdentityProviderPluginProxy::new(v, lib.clone()));
    }

    Ok(LoadedPlugin {
        source_plugins,
        reaction_plugins,
        bootstrap_plugins,
        identity_provider_plugins,
        metadata_info,
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

/// Simple glob-like pattern matching (supports `*` wildcards).
fn matches_pattern(filename: &str, pattern: &str) -> bool {
    let known_exts = [".dylib", ".so", ".dll"];

    // Strip known extension from filename and pattern so matching is platform-agnostic
    let stem = known_exts
        .iter()
        .find_map(|ext| filename.strip_suffix(ext))
        .unwrap_or(filename);
    let pat = known_exts
        .iter()
        .find_map(|ext| pattern.strip_suffix(ext))
        .unwrap_or(pattern);

    // Simple wildcard matching
    if let Some(prefix) = pat.strip_suffix('*') {
        stem.starts_with(prefix)
    } else if let Some((prefix, suffix)) = pat.split_once('*') {
        stem.starts_with(prefix) && stem.ends_with(suffix)
    } else {
        stem == pat
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_pattern_prefix_wildcard() {
        assert!(matches_pattern(
            "libdrasi_source_mock.so",
            "libdrasi_source_*"
        ));
        assert!(matches_pattern(
            "libdrasi_source_http.so",
            "libdrasi_source_*"
        ));
        assert!(!matches_pattern(
            "libdrasi_reaction_log.so",
            "libdrasi_source_*"
        ));
    }

    #[test]
    fn test_matches_pattern_exact() {
        assert!(matches_pattern(
            "libdrasi_source_mock.so",
            "libdrasi_source_mock"
        ));
        assert!(!matches_pattern(
            "libdrasi_source_http.so",
            "libdrasi_source_mock"
        ));
    }

    #[test]
    fn test_matches_pattern_middle_wildcard() {
        assert!(matches_pattern("libdrasi_source_mock.so", "lib*mock.so"));
        assert!(!matches_pattern("libdrasi_source_http.so", "lib*mock.so"));
    }
}
