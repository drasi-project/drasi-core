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

//! ScriptFile bootstrap plugin for Drasi
//!
//! This plugin provides the ScriptFile bootstrap provider implementation and extension
//! traits for creating ScriptFile bootstrap providers in the Drasi plugin architecture.

pub mod script_file;
pub mod script_reader;
pub mod script_types;

pub use drasi_lib::bootstrap::{ScriptFileBootstrapConfig, BootstrapProviderConfig};
pub use script_file::ScriptFileBootstrapProvider;

/// Extension trait for creating ScriptFile bootstrap providers
///
/// This trait is implemented on `BootstrapProviderConfig` to provide a fluent builder API
/// for configuring ScriptFile bootstrap providers that read bootstrap data from JSONL files.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::bootstrap::BootstrapProviderConfig;
/// use drasi_bootstrap_scriptfile::BootstrapProviderConfigScriptFileExt;
///
/// let config = BootstrapProviderConfig::scriptfile()
///     .add_file_path("/path/to/data.jsonl")
///     .build();
/// ```
pub trait BootstrapProviderConfigScriptFileExt {
    /// Create a new ScriptFile bootstrap provider configuration builder
    fn scriptfile() -> ScriptFileBootstrapBuilder;
}

/// Builder for ScriptFile bootstrap provider configuration
pub struct ScriptFileBootstrapBuilder {
    file_paths: Vec<String>,
}

impl ScriptFileBootstrapBuilder {
    /// Create a new ScriptFile bootstrap provider builder
    pub fn new() -> Self {
        Self {
            file_paths: Vec::new(),
        }
    }

    /// Set the file paths
    pub fn with_file_paths(mut self, paths: Vec<String>) -> Self {
        self.file_paths = paths;
        self
    }

    /// Add a single file path
    pub fn add_file_path(mut self, path: impl Into<String>) -> Self {
        self.file_paths.push(path.into());
        self
    }

    /// Build the ScriptFile bootstrap provider configuration
    pub fn build(self) -> ScriptFileBootstrapConfig {
        ScriptFileBootstrapConfig {
            file_paths: self.file_paths,
        }
    }
}

impl Default for ScriptFileBootstrapBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BootstrapProviderConfigScriptFileExt for BootstrapProviderConfig {
    fn scriptfile() -> ScriptFileBootstrapBuilder {
        ScriptFileBootstrapBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scriptfile_bootstrap_builder_empty() {
        let config = ScriptFileBootstrapBuilder::new().build();
        assert_eq!(config.file_paths.len(), 0);
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_single_file() {
        let config = ScriptFileBootstrapBuilder::new()
            .add_file_path("/path/to/file.jsonl")
            .build();

        assert_eq!(config.file_paths.len(), 1);
        assert_eq!(config.file_paths[0], "/path/to/file.jsonl");
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_multiple_files() {
        let config = ScriptFileBootstrapBuilder::new()
            .add_file_path("/path/to/file1.jsonl")
            .add_file_path("/path/to/file2.jsonl")
            .add_file_path("/path/to/file3.jsonl")
            .build();

        assert_eq!(config.file_paths.len(), 3);
        assert_eq!(config.file_paths[0], "/path/to/file1.jsonl");
        assert_eq!(config.file_paths[1], "/path/to/file2.jsonl");
        assert_eq!(config.file_paths[2], "/path/to/file3.jsonl");
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_with_file_paths() {
        let paths = vec![
            "/data/nodes.jsonl".to_string(),
            "/data/relations.jsonl".to_string(),
        ];
        let config = ScriptFileBootstrapBuilder::new()
            .with_file_paths(paths.clone())
            .build();

        assert_eq!(config.file_paths, paths);
    }

    #[test]
    fn test_scriptfile_bootstrap_extension_trait() {
        let config = BootstrapProviderConfig::scriptfile()
            .add_file_path("/initial/data.jsonl")
            .build();

        assert_eq!(config.file_paths.len(), 1);
        assert_eq!(config.file_paths[0], "/initial/data.jsonl");
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_default() {
        let config = ScriptFileBootstrapBuilder::default().build();
        assert_eq!(config.file_paths.len(), 0);
    }

    #[test]
    fn test_scriptfile_bootstrap_fluent_api() {
        let config = BootstrapProviderConfig::scriptfile()
            .add_file_path("/bootstrap/nodes.jsonl")
            .add_file_path("/bootstrap/relations.jsonl")
            .build();

        assert_eq!(config.file_paths.len(), 2);
        assert_eq!(config.file_paths[0], "/bootstrap/nodes.jsonl");
        assert_eq!(config.file_paths[1], "/bootstrap/relations.jsonl");
    }
}
