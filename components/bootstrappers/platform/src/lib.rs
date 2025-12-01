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
//! This plugin provides the Platform bootstrap provider implementation and extension
//! traits for creating Platform bootstrap providers in the Drasi plugin architecture.

pub mod platform;

pub use drasi_lib::bootstrap::{PlatformBootstrapConfig, BootstrapProviderConfig};
pub use platform::PlatformBootstrapProvider;

/// Extension trait for creating Platform bootstrap providers
///
/// This trait is implemented on `BootstrapProviderConfig` to provide a fluent builder API
/// for configuring Platform bootstrap providers that fetch initial data from a remote
/// Drasi Query API service.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::bootstrap::BootstrapProviderConfig;
/// use drasi_bootstrap_platform::BootstrapProviderConfigPlatformExt;
///
/// let config = BootstrapProviderConfig::platform()
///     .with_query_api_url("http://remote-drasi:8080")
///     .with_timeout_seconds(600)
///     .build();
/// ```
pub trait BootstrapProviderConfigPlatformExt {
    /// Create a new Platform bootstrap provider configuration builder
    fn platform() -> PlatformBootstrapBuilder;
}

/// Builder for Platform bootstrap provider configuration
pub struct PlatformBootstrapBuilder {
    query_api_url: Option<String>,
    timeout_seconds: u64,
}

impl PlatformBootstrapBuilder {
    /// Create a new Platform bootstrap provider builder
    pub fn new() -> Self {
        Self {
            query_api_url: None,
            timeout_seconds: 300, // Default timeout
        }
    }

    /// Set the Query API URL
    pub fn with_query_api_url(mut self, url: impl Into<String>) -> Self {
        self.query_api_url = Some(url.into());
        self
    }

    /// Set the timeout in seconds
    pub fn with_timeout_seconds(mut self, seconds: u64) -> Self {
        self.timeout_seconds = seconds;
        self
    }

    /// Build the Platform bootstrap provider configuration
    pub fn build(self) -> PlatformBootstrapConfig {
        PlatformBootstrapConfig {
            query_api_url: self.query_api_url,
            timeout_seconds: self.timeout_seconds,
        }
    }
}

impl Default for PlatformBootstrapBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BootstrapProviderConfigPlatformExt for BootstrapProviderConfig {
    fn platform() -> PlatformBootstrapBuilder {
        PlatformBootstrapBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_bootstrap_builder_defaults() {
        let config = PlatformBootstrapBuilder::new().build();
        assert_eq!(config.query_api_url, None);
        assert_eq!(config.timeout_seconds, 300);
    }

    #[test]
    fn test_platform_bootstrap_builder_custom_values() {
        let config = PlatformBootstrapBuilder::new()
            .with_query_api_url("http://remote-drasi:8080")
            .with_timeout_seconds(600)
            .build();

        assert_eq!(config.query_api_url, Some("http://remote-drasi:8080".to_string()));
        assert_eq!(config.timeout_seconds, 600);
    }

    #[test]
    fn test_platform_bootstrap_extension_trait() {
        let config = BootstrapProviderConfig::platform()
            .with_query_api_url("http://localhost:8080")
            .build();

        assert_eq!(config.query_api_url, Some("http://localhost:8080".to_string()));
    }

    #[test]
    fn test_platform_bootstrap_builder_default() {
        let config = PlatformBootstrapBuilder::default().build();
        assert_eq!(config.query_api_url, None);
        assert_eq!(config.timeout_seconds, 300);
    }

    #[test]
    fn test_platform_bootstrap_fluent_api() {
        let config = BootstrapProviderConfig::platform()
            .with_query_api_url("http://source-api:9000")
            .with_timeout_seconds(900)
            .build();

        assert_eq!(config.query_api_url, Some("http://source-api:9000".to_string()));
        assert_eq!(config.timeout_seconds, 900);
    }
}
