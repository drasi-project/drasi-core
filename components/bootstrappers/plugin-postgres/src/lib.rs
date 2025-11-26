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

//! PostgreSQL bootstrap plugin for Drasi
//!
//! This plugin provides the PostgreSQL bootstrap provider implementation and extension
//! traits for creating PostgreSQL bootstrap providers in the Drasi plugin architecture.

pub mod postgres;

pub use drasi_lib::bootstrap::{PostgresBootstrapConfig, BootstrapProviderConfig};
pub use postgres::PostgresBootstrapProvider;

/// Extension trait for creating PostgreSQL bootstrap providers
///
/// This trait is implemented on `BootstrapProviderConfig` to provide a fluent builder API
/// for configuring PostgreSQL bootstrap providers.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::bootstrap::BootstrapProviderConfig;
/// use drasi_plugin_postgres_bootstrap::BootstrapProviderConfigPostgresExt;
///
/// let config = BootstrapProviderConfig::postgres().build();
/// ```
pub trait BootstrapProviderConfigPostgresExt {
    /// Create a new PostgreSQL bootstrap provider configuration builder
    fn postgres() -> PostgresBootstrapBuilder;
}

/// Builder for PostgreSQL bootstrap provider configuration
pub struct PostgresBootstrapBuilder;

impl PostgresBootstrapBuilder {
    /// Create a new PostgreSQL bootstrap provider builder
    pub fn new() -> Self {
        Self
    }

    /// Build the PostgreSQL bootstrap provider configuration
    pub fn build(self) -> PostgresBootstrapConfig {
        PostgresBootstrapConfig::default()
    }
}

impl Default for PostgresBootstrapBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl BootstrapProviderConfigPostgresExt for BootstrapProviderConfig {
    fn postgres() -> PostgresBootstrapBuilder {
        PostgresBootstrapBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_postgres_bootstrap_builder() {
        let config = PostgresBootstrapBuilder::new().build();
        assert_eq!(config, PostgresBootstrapConfig::default());
    }

    #[test]
    fn test_postgres_bootstrap_extension_trait() {
        let config = BootstrapProviderConfig::postgres().build();
        assert_eq!(config, PostgresBootstrapConfig::default());
    }

    #[test]
    fn test_postgres_bootstrap_builder_default() {
        let config = PostgresBootstrapBuilder::default().build();
        assert_eq!(config, PostgresBootstrapConfig::default());
    }
}
