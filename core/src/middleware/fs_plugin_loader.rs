// Copyright 2024 The Drasi Authors.
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

//! File system-based plugin loader for discovering middleware plugins.
//!
//! This module provides a basic implementation of the PluginLoader trait that
//! discovers plugins by scanning a directory on the file system.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::plugin_loader::{
    PluginDescriptor, PluginFormat, PluginLoadError, PluginLoader, PluginLoaderConfig,
    PluginMetadata,
};
use crate::interface::SourceMiddlewareFactory;

/// Error message for unimplemented plugin loading
const PLUGIN_LOADING_NOT_IMPLEMENTED: &str = 
    "Plugin loading not yet implemented. This requires format-specific loaders.";

/// File system-based plugin loader that discovers plugins in a directory
pub struct FileSystemPluginLoader {
    config: PluginLoaderConfig,
}

impl FileSystemPluginLoader {
    /// Creates a new file system plugin loader with the given configuration
    pub fn new(config: PluginLoaderConfig) -> Self {
        Self { config }
    }

    /// Creates a new file system plugin loader for the specified directory
    pub fn with_directory<P: Into<PathBuf>>(plugin_dir: P) -> Self {
        Self {
            config: PluginLoaderConfig {
                plugin_dir: plugin_dir.into(),
                ..Default::default()
            },
        }
    }

    /// Scans a directory for plugin files
    fn scan_directory(&self, dir: &Path) -> Result<Vec<PathBuf>, PluginLoadError> {
        if !dir.exists() {
            log::debug!(
                "Plugin directory does not exist: {}",
                dir.display()
            );
            return Ok(Vec::new());
        }

        if !dir.is_dir() {
            return Err(PluginLoadError::InvalidFormat(format!(
                "Path is not a directory: {}",
                dir.display()
            )));
        }

        let mut plugin_files = Vec::new();

        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if self.config.recursive && path.is_dir() {
                // Recursively scan subdirectories
                plugin_files.extend(self.scan_directory(&path)?);
            } else if path.is_file() {
                // Check if this is a supported plugin format
                if let Some(format) = self.detect_plugin_format(&path) {
                    if self.config.supported_formats.contains(&format) {
                        plugin_files.push(path);
                    }
                }
            }
        }

        Ok(plugin_files)
    }

    /// Detects the plugin format based on file extension
    fn detect_plugin_format(&self, path: &Path) -> Option<PluginFormat> {
        path.extension().and_then(|ext| {
            let ext = ext.to_string_lossy().to_lowercase();
            match ext.as_str() {
                "wasm" => Some(PluginFormat::Wasm),
                "so" | "dll" | "dylib" => Some(PluginFormat::DynamicLibrary),
                _ => None,
            }
        })
    }

    /// Creates a plugin descriptor from a file path
    fn create_descriptor(&self, path: PathBuf) -> Result<PluginDescriptor, PluginLoadError> {
        let format = self
            .detect_plugin_format(&path)
            .ok_or_else(|| {
                PluginLoadError::InvalidFormat(format!("Unknown plugin format: {}", path.display()))
            })?;

        // Extract plugin name from filename (without extension)
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| {
                PluginLoadError::InvalidFormat(format!("Invalid filename: {}", path.display()))
            })?
            .to_string();

        // For now, we use basic metadata. In the future, this could be enhanced
        // to read from a manifest file or embedded metadata
        let descriptor = PluginDescriptor {
            name,
            version: "unknown".to_string(),
            path,
            format,
            metadata: PluginMetadata::default(),
        };

        Ok(descriptor)
    }
}

impl PluginLoader for FileSystemPluginLoader {
    fn discover_plugins(&self) -> Result<Vec<PluginDescriptor>, PluginLoadError> {
        let plugin_files = self.scan_directory(&self.config.plugin_dir)?;

        let mut descriptors = Vec::new();
        for path in plugin_files {
            match self.create_descriptor(path) {
                Ok(descriptor) => descriptors.push(descriptor),
                Err(e) => {
                    if self.config.fail_on_error {
                        return Err(e);
                    } else {
                        log::warn!("Failed to create plugin descriptor: {}", e);
                    }
                }
            }
        }

        Ok(descriptors)
    }

    fn load_plugin(
        &mut self,
        _descriptor: &PluginDescriptor,
    ) -> Result<Arc<dyn SourceMiddlewareFactory>, PluginLoadError> {
        // This is a stub implementation. The actual loading logic would be
        // implemented in format-specific loaders (WASM, dynamic library, etc.)
        Err(PluginLoadError::InitializationFailed(
            PLUGIN_LOADING_NOT_IMPLEMENTED.to_string(),
        ))
    }

    fn loader_name(&self) -> &str {
        "filesystem"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, File};
    use tempfile::TempDir;

    #[test]
    fn test_discover_plugins_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let loader = FileSystemPluginLoader::with_directory(temp_dir.path());

        let result = loader.discover_plugins().unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_discover_plugins_with_wasm_file() {
        let temp_dir = TempDir::new().unwrap();
        let plugin_path = temp_dir.path().join("test_plugin.wasm");
        File::create(&plugin_path).unwrap();

        let loader = FileSystemPluginLoader::with_directory(temp_dir.path());

        let result = loader.discover_plugins().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "test_plugin");
        assert_eq!(result[0].format, PluginFormat::Wasm);
    }

    #[test]
    fn test_discover_plugins_ignores_unsupported_formats() {
        let temp_dir = TempDir::new().unwrap();
        File::create(temp_dir.path().join("test1.wasm")).unwrap();
        File::create(temp_dir.path().join("test2.txt")).unwrap();
        File::create(temp_dir.path().join("test3.dll")).unwrap(); // Not in default supported formats

        let loader = FileSystemPluginLoader::with_directory(temp_dir.path());

        let result = loader.discover_plugins().unwrap();
        assert_eq!(result.len(), 1); // Only .wasm file should be discovered
        assert_eq!(result[0].name, "test1");
    }

    #[test]
    fn test_discover_plugins_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let subdir = temp_dir.path().join("subdir");
        create_dir_all(&subdir).unwrap();

        File::create(temp_dir.path().join("plugin1.wasm")).unwrap();
        File::create(subdir.join("plugin2.wasm")).unwrap();

        let config = PluginLoaderConfig {
            plugin_dir: temp_dir.path().to_path_buf(),
            recursive: true,
            ..Default::default()
        };
        let loader = FileSystemPluginLoader::new(config);

        let result = loader.discover_plugins().unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_discover_plugins_non_recursive() {
        let temp_dir = TempDir::new().unwrap();
        let subdir = temp_dir.path().join("subdir");
        create_dir_all(&subdir).unwrap();

        File::create(temp_dir.path().join("plugin1.wasm")).unwrap();
        File::create(subdir.join("plugin2.wasm")).unwrap();

        let config = PluginLoaderConfig {
            plugin_dir: temp_dir.path().to_path_buf(),
            recursive: false,
            ..Default::default()
        };
        let loader = FileSystemPluginLoader::new(config);

        let result = loader.discover_plugins().unwrap();
        assert_eq!(result.len(), 1); // Only root level plugin
        assert_eq!(result[0].name, "plugin1");
    }

    #[test]
    fn test_nonexistent_directory() {
        let loader = FileSystemPluginLoader::with_directory("/nonexistent/path/to/plugins");

        let result = loader.discover_plugins().unwrap();
        assert_eq!(result.len(), 0); // Should return empty list, not error
    }

    #[test]
    fn test_detect_plugin_format() {
        let temp_dir = TempDir::new().unwrap();
        let loader = FileSystemPluginLoader::with_directory(temp_dir.path());

        assert_eq!(
            loader.detect_plugin_format(Path::new("test.wasm")),
            Some(PluginFormat::Wasm)
        );
        assert_eq!(
            loader.detect_plugin_format(Path::new("test.so")),
            Some(PluginFormat::DynamicLibrary)
        );
        assert_eq!(
            loader.detect_plugin_format(Path::new("test.dll")),
            Some(PluginFormat::DynamicLibrary)
        );
        assert_eq!(
            loader.detect_plugin_format(Path::new("test.dylib")),
            Some(PluginFormat::DynamicLibrary)
        );
        assert_eq!(loader.detect_plugin_format(Path::new("test.txt")), None);
    }
}
