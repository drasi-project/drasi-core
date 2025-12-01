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

//! Application bootstrap plugin for Drasi
//!
//! This plugin provides the Application bootstrap provider implementation and extension
//! traits for creating Application bootstrap providers in the Drasi plugin architecture.

pub mod application;

pub use drasi_lib::bootstrap::{ApplicationBootstrapConfig, BootstrapProviderConfig};
pub use application::ApplicationBootstrapProvider;

/// Extension trait for creating Application bootstrap providers
///
/// This trait is implemented on `BootstrapProviderConfig` to provide a fluent builder API
/// for configuring Application bootstrap providers that replay stored insert events.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::bootstrap::BootstrapProviderConfig;
/// use drasi_bootstrap_application::BootstrapProviderConfigApplicationExt;
///
/// let config = BootstrapProviderConfig::application().build();
/// ```
pub trait BootstrapProviderConfigApplicationExt {
    /// Create a new Application bootstrap provider configuration builder
    fn application() -> ApplicationBootstrapBuilder;
}

/// Builder for Application bootstrap provider configuration
pub struct ApplicationBootstrapBuilder;

impl ApplicationBootstrapBuilder {
    /// Create a new Application bootstrap provider builder
    pub fn new() -> Self {
        Self
    }

    /// Build the Application bootstrap provider configuration
    pub fn build(self) -> ApplicationBootstrapConfig {
        ApplicationBootstrapConfig::default()
    }
}

impl Default for ApplicationBootstrapBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BootstrapProviderConfigApplicationExt for BootstrapProviderConfig {
    fn application() -> ApplicationBootstrapBuilder {
        ApplicationBootstrapBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_application_bootstrap_builder() {
        let config = ApplicationBootstrapBuilder::new().build();
        assert_eq!(config, ApplicationBootstrapConfig::default());
    }

    #[test]
    fn test_application_bootstrap_extension_trait() {
        let config = BootstrapProviderConfig::application().build();
        assert_eq!(config, ApplicationBootstrapConfig::default());
    }

    #[test]
    fn test_application_bootstrap_builder_default() {
        let config = ApplicationBootstrapBuilder::default().build();
        assert_eq!(config, ApplicationBootstrapConfig::default());
    }
}
