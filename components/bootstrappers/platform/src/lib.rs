#![allow(unexpected_cfgs)]
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

//! Platform bootstrap plugin for Drasi
//!
//! This plugin provides the Platform bootstrap provider implementation for fetching
//! initial data from a remote Drasi Query API service.
//!
//! # Example
//!
//! ```no_run
//! use drasi_bootstrap_platform::PlatformBootstrapProvider;
//!
//! // Using the builder
//! let provider = PlatformBootstrapProvider::builder()
//!     .with_query_api_url("http://remote-drasi:8080")
//!     .with_timeout_seconds(600)
//!     .build()
//!     .expect("Failed to create provider");
//!
//! // Or using configuration
//! use drasi_lib::bootstrap::PlatformBootstrapConfig;
//!
//! let config = PlatformBootstrapConfig {
//!     query_api_url: Some("http://remote-drasi:8080".to_string()),
//!     timeout_seconds: 600,
//! };
//! let provider = PlatformBootstrapProvider::new(config)
//!     .expect("Failed to create provider");
//! ```

pub mod descriptor;
pub mod platform;

pub use drasi_lib::bootstrap::PlatformBootstrapConfig;
pub use platform::{PlatformBootstrapProvider, PlatformBootstrapProviderBuilder};

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "platform-bootstrap",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::PlatformBootstrapDescriptor],
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_bootstrap_builder_requires_url() {
        // Builder without URL should fail to build
        let result = PlatformBootstrapProviderBuilder::new().build();
        assert!(result.is_err());
    }

    #[test]
    fn test_platform_bootstrap_builder_with_valid_url() {
        // Builder with valid URL should succeed
        let result = PlatformBootstrapProviderBuilder::new()
            .with_query_api_url("http://remote-drasi:8080") // DevSkim: ignore DS137138
            .with_timeout_seconds(600)
            .build();
        assert!(result.is_ok());
    }

    #[test]
    fn test_platform_bootstrap_builder_invalid_url() {
        // Builder with invalid URL should fail
        let result = PlatformBootstrapProviderBuilder::new()
            .with_query_api_url("not-a-valid-url")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn test_platform_bootstrap_builder_default() {
        // Default builder should have no URL set
        let builder = PlatformBootstrapProviderBuilder::default();
        // Without URL, build should fail
        let result = builder.build();
        assert!(result.is_err());
    }

    #[test]
    fn test_platform_bootstrap_from_provider_method() {
        // Test using PlatformBootstrapProvider::builder()
        let result = PlatformBootstrapProvider::builder()
            .with_query_api_url("http://source-api:9000") // DevSkim: ignore DS137138
            .with_timeout_seconds(900)
            .build();
        assert!(result.is_ok());
    }

    #[test]
    fn test_platform_bootstrap_new_with_config() {
        // Test using PlatformBootstrapProvider::new(config)
        let config = PlatformBootstrapConfig {
            query_api_url: Some("http://localhost:8080".to_string()), // DevSkim: ignore DS137138
            timeout_seconds: 300,
        };
        let result = PlatformBootstrapProvider::new(config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_platform_bootstrap_new_without_url() {
        // Config without URL should fail
        let config = PlatformBootstrapConfig {
            query_api_url: None,
            timeout_seconds: 300,
        };
        let result = PlatformBootstrapProvider::new(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_platform_bootstrap_with_url() {
        // Test using PlatformBootstrapProvider::with_url()
        let result = PlatformBootstrapProvider::with_url("http://example.com:8080", 600); // DevSkim: ignore DS137138
        assert!(result.is_ok());
    }
}
