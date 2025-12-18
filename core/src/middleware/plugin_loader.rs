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

//! Plugin loading infrastructure for extensible middleware system.
//!
//! This module provides the foundation for loading external middleware plugins,
//! enabling developers to create and distribute middleware components independently
//! of the core library.

use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

use crate::interface::{MiddlewareSetupError, SourceMiddlewareFactory};

/// Errors that can occur during plugin loading
#[derive(Error, Debug)]
pub enum PluginLoadError {
    #[error("Plugin file not found: {0}")]
    PluginNotFound(PathBuf),

    #[error("Invalid plugin format: {0}")]
    InvalidFormat(String),

    #[error("Plugin version mismatch: {0}")]
    VersionMismatch(String),

    #[error("Plugin initialization failed: {0}")]
    InitializationFailed(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Trait for plugin loaders that can load middleware factories from external sources
pub trait PluginLoader: Send + Sync {
    /// Discovers plugins in the configured location
    fn discover_plugins(&self) -> Result<Vec<PluginDescriptor>, PluginLoadError>;

    /// Loads a specific plugin and returns its factory
    fn load_plugin(
        &mut self,
        descriptor: &PluginDescriptor,
    ) -> Result<Arc<dyn SourceMiddlewareFactory>, PluginLoadError>;

    /// Gets the name/identifier of this loader
    fn loader_name(&self) -> &str;
}

/// Information about a discovered plugin
#[derive(Debug, Clone)]
pub struct PluginDescriptor {
    /// Name of the plugin
    pub name: String,

    /// Version of the plugin
    pub version: String,

    /// Path to the plugin file
    pub path: PathBuf,

    /// Plugin format (e.g., "wasm", "dylib")
    pub format: PluginFormat,

    /// Additional metadata about the plugin
    pub metadata: PluginMetadata,
}

/// Format of the plugin
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginFormat {
    /// WebAssembly plugin
    Wasm,

    /// Dynamic library plugin (platform-specific)
    DynamicLibrary,

    /// Future extension point for other formats
    Other(String),
}

/// Additional metadata about a plugin
#[derive(Debug, Clone, Default)]
pub struct PluginMetadata {
    /// Plugin author
    pub author: Option<String>,

    /// Plugin description
    pub description: Option<String>,

    /// Plugin homepage or repository URL
    pub homepage: Option<String>,

    /// Required API version
    pub api_version: Option<String>,
}

/// Configuration for plugin loading
#[derive(Debug, Clone)]
pub struct PluginLoaderConfig {
    /// Directory to scan for plugins
    pub plugin_dir: PathBuf,

    /// Whether to load plugins recursively from subdirectories
    pub recursive: bool,

    /// List of plugin formats to support
    pub supported_formats: Vec<PluginFormat>,

    /// Whether to fail if any plugin fails to load
    pub fail_on_error: bool,
}

impl Default for PluginLoaderConfig {
    fn default() -> Self {
        Self {
            plugin_dir: PathBuf::from("./plugins"),
            recursive: false,
            supported_formats: vec![PluginFormat::Wasm],
            fail_on_error: false,
        }
    }
}

/// Converts plugin load errors to middleware setup errors
impl From<PluginLoadError> for MiddlewareSetupError {
    fn from(err: PluginLoadError) -> Self {
        MiddlewareSetupError::InvalidConfiguration(format!("Plugin load error: {}", err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_descriptor_creation() {
        let descriptor = PluginDescriptor {
            name: "test-plugin".to_string(),
            version: "1.0.0".to_string(),
            path: PathBuf::from("/plugins/test.wasm"),
            format: PluginFormat::Wasm,
            metadata: PluginMetadata {
                author: Some("Test Author".to_string()),
                description: Some("A test plugin".to_string()),
                homepage: None,
                api_version: Some("1.0".to_string()),
            },
        };

        assert_eq!(descriptor.name, "test-plugin");
        assert_eq!(descriptor.format, PluginFormat::Wasm);
    }

    #[test]
    fn test_default_plugin_loader_config() {
        let config = PluginLoaderConfig::default();
        assert_eq!(config.plugin_dir, PathBuf::from("./plugins"));
        assert!(!config.recursive);
        assert!(!config.fail_on_error);
        assert_eq!(config.supported_formats.len(), 1);
    }
}
