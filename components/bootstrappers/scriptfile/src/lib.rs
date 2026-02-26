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
//! This plugin provides the ScriptFile bootstrap provider implementation for reading
//! bootstrap data from JSONL script files.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;
//!
//! // Using the builder
//! let provider = ScriptFileBootstrapProvider::builder()
//!     .with_file("/path/to/data.jsonl")
//!     .with_file("/path/to/more_data.jsonl")
//!     .build();
//!
//! // Or using configuration
//! use drasi_lib::bootstrap::ScriptFileBootstrapConfig;
//!
//! let config = ScriptFileBootstrapConfig {
//!     file_paths: vec!["/path/to/data.jsonl".to_string()],
//! };
//! let provider = ScriptFileBootstrapProvider::new(config);
//!
//! // Or using with_paths directly
//! let provider = ScriptFileBootstrapProvider::with_paths(vec![
//!     "/path/to/data.jsonl".to_string()
//! ]);
//! ```

pub mod descriptor;
pub mod script_file;
pub mod script_reader;
pub mod script_types;

pub use drasi_lib::bootstrap::ScriptFileBootstrapConfig;
pub use script_file::{ScriptFileBootstrapProvider, ScriptFileBootstrapProviderBuilder};

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "scriptfile-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::ScriptFileBootstrapDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scriptfile_bootstrap_builder_empty() {
        let provider = ScriptFileBootstrapProviderBuilder::new().build();
        // Provider created with empty file paths
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_single_file() {
        let provider = ScriptFileBootstrapProviderBuilder::new()
            .with_file("/path/to/file.jsonl")
            .build();
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_multiple_files() {
        let provider = ScriptFileBootstrapProviderBuilder::new()
            .with_file("/path/to/file1.jsonl")
            .with_file("/path/to/file2.jsonl")
            .with_file("/path/to/file3.jsonl")
            .build();
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_with_file_paths() {
        let paths = vec![
            "/data/nodes.jsonl".to_string(),
            "/data/relations.jsonl".to_string(),
        ];
        let provider = ScriptFileBootstrapProviderBuilder::new()
            .with_file_paths(paths)
            .build();
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_from_provider_method() {
        // Test using ScriptFileBootstrapProvider::builder()
        let provider = ScriptFileBootstrapProvider::builder()
            .with_file("/initial/data.jsonl")
            .build();
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_builder_default() {
        let provider = ScriptFileBootstrapProviderBuilder::default().build();
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_provider_default() {
        // ScriptFileBootstrapProvider::default() should work
        let provider = ScriptFileBootstrapProvider::default();
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_new_with_config() {
        // Test using ScriptFileBootstrapProvider::new(config)
        let config = ScriptFileBootstrapConfig {
            file_paths: vec!["/bootstrap/nodes.jsonl".to_string()],
        };
        let provider = ScriptFileBootstrapProvider::new(config);
        let _ = provider;
    }

    #[test]
    fn test_scriptfile_bootstrap_with_paths() {
        // Test using ScriptFileBootstrapProvider::with_paths()
        let provider = ScriptFileBootstrapProvider::with_paths(vec![
            "/bootstrap/nodes.jsonl".to_string(),
            "/bootstrap/relations.jsonl".to_string(),
        ]);
        let _ = provider;
    }
}
