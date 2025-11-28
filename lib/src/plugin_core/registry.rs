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

//! Plugin Registry Module
//!
//! This module provides factory registries for creating source and reaction instances
//! from generic configuration. This enables dynamic plugin creation based on type names.
//!
//! # Architecture
//!
//! The registry pattern allows applications (like drasi-server) to:
//! 1. Register factory functions for each plugin type
//! 2. Create instances from generic `SourceConfig` or `ReactionConfig`
//! 3. Support dynamic plugin loading based on configuration
//!
//! # Example
//!
//! ```ignore
//! use drasi_lib::plugin_core::{SourceRegistry, Source};
//! use drasi_lib::config::SourceConfig;
//!
//! let mut registry = SourceRegistry::new();
//!
//! // Register a factory for "mock" source type
//! registry.register("mock", |config| {
//!     let source = MockSource::from_config(config)?;
//!     Ok(Arc::new(source) as Arc<dyn Source>)
//! });
//!
//! // Create a source from config
//! let config = SourceConfig::new("my-source", "mock");
//! let source = registry.create(&config)?;
//! ```

use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;

use super::{Reaction, Source};
use crate::config::{ReactionConfig, SourceConfig};

/// Factory function type for creating source instances from configuration.
///
/// The factory receives a `SourceConfig` and should create an appropriate source instance.
pub type SourceFactory = Box<dyn Fn(&SourceConfig) -> Result<Arc<dyn Source>> + Send + Sync>;

/// Factory function type for creating reaction instances from configuration.
///
/// The factory receives a `ReactionConfig` and should create an appropriate reaction instance.
pub type ReactionFactory = Box<dyn Fn(&ReactionConfig) -> Result<Arc<dyn Reaction>> + Send + Sync>;

/// Registry for source plugin factories.
///
/// `SourceRegistry` maintains a mapping from source type names to factory functions
/// that can create source instances from generic configuration.
///
/// # Example
///
/// ```ignore
/// let mut registry = SourceRegistry::new();
///
/// registry.register("postgres", |config| {
///     let pg_config = PostgresSourceConfig::from_properties(&config.properties)?;
///     let source = PostgresSource::new(&config.id, pg_config)?;
///     Ok(Arc::new(source) as Arc<dyn Source>)
/// });
///
/// let config = SourceConfig::new("my-db", "postgres")
///     .with_property("host", "localhost");
/// let source = registry.create(&config)?;
/// ```
pub struct SourceRegistry {
    factories: HashMap<String, SourceFactory>,
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceRegistry {
    /// Create a new empty source registry.
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Register a factory function for a source type.
    ///
    /// # Arguments
    ///
    /// * `source_type` - The type name (e.g., "postgres", "http", "mock")
    /// * `factory` - Factory function that creates source instances
    pub fn register<F>(&mut self, source_type: impl Into<String>, factory: F)
    where
        F: Fn(&SourceConfig) -> Result<Arc<dyn Source>> + Send + Sync + 'static,
    {
        self.factories.insert(source_type.into(), Box::new(factory));
    }

    /// Create a source instance from configuration.
    ///
    /// Looks up the factory for the config's `source_type` and calls it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No factory is registered for the source type
    /// - The factory returns an error
    pub fn create(&self, config: &SourceConfig) -> Result<Arc<dyn Source>> {
        let factory = self
            .factories
            .get(&config.source_type)
            .ok_or_else(|| anyhow!("Unknown source type: '{}'", config.source_type))?;

        factory(config)
    }

    /// Check if a factory is registered for a source type.
    pub fn has_factory(&self, source_type: &str) -> bool {
        self.factories.contains_key(source_type)
    }

    /// Get a list of registered source types.
    pub fn registered_types(&self) -> Vec<&str> {
        self.factories.keys().map(|s| s.as_str()).collect()
    }
}

/// Registry for reaction plugin factories.
///
/// `ReactionRegistry` maintains a mapping from reaction type names to factory functions
/// that can create reaction instances from generic configuration.
///
/// # Example
///
/// ```ignore
/// let mut registry = ReactionRegistry::new();
///
/// registry.register("http", |config| {
///     let http_config = HttpReactionConfig::from_properties(&config.properties)?;
///     let reaction = HttpReaction::new(&config.id, config.queries.clone(), http_config)?;
///     Ok(Arc::new(reaction) as Arc<dyn Reaction>)
/// });
///
/// let config = ReactionConfig::new("my-webhook", "http")
///     .with_query("query1")
///     .with_property("endpoint", "http://localhost:8080");
/// let reaction = registry.create(&config)?;
/// ```
pub struct ReactionRegistry {
    factories: HashMap<String, ReactionFactory>,
}

impl Default for ReactionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ReactionRegistry {
    /// Create a new empty reaction registry.
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Register a factory function for a reaction type.
    ///
    /// # Arguments
    ///
    /// * `reaction_type` - The type name (e.g., "log", "http", "grpc")
    /// * `factory` - Factory function that creates reaction instances
    pub fn register<F>(&mut self, reaction_type: impl Into<String>, factory: F)
    where
        F: Fn(&ReactionConfig) -> Result<Arc<dyn Reaction>> + Send + Sync + 'static,
    {
        self.factories
            .insert(reaction_type.into(), Box::new(factory));
    }

    /// Create a reaction instance from configuration.
    ///
    /// Looks up the factory for the config's `reaction_type` and calls it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - No factory is registered for the reaction type
    /// - The factory returns an error
    pub fn create(&self, config: &ReactionConfig) -> Result<Arc<dyn Reaction>> {
        let factory = self
            .factories
            .get(&config.reaction_type)
            .ok_or_else(|| anyhow!("Unknown reaction type: '{}'", config.reaction_type))?;

        factory(config)
    }

    /// Check if a factory is registered for a reaction type.
    pub fn has_factory(&self, reaction_type: &str) -> bool {
        self.factories.contains_key(reaction_type)
    }

    /// Get a list of registered reaction types.
    pub fn registered_types(&self) -> Vec<&str> {
        self.factories.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::{ComponentEventSender, ComponentStatus, SubscriptionResponse};
    use async_trait::async_trait;
    use std::collections::HashMap;

    // Mock source for testing
    struct MockTestSource {
        id: String,
    }

    #[async_trait]
    impl Source for MockTestSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "mock"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        async fn start(&self) -> Result<()> {
            Ok(())
        }

        async fn stop(&self) -> Result<()> {
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            ComponentStatus::Stopped
        }

        async fn subscribe(
            &self,
            _query_id: String,
            _enable_bootstrap: bool,
            _node_labels: Vec<String>,
            _relation_labels: Vec<String>,
        ) -> Result<SubscriptionResponse> {
            unimplemented!()
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        async fn inject_event_tx(&self, _tx: ComponentEventSender) {}
    }

    #[test]
    fn test_source_registry_register_and_create() {
        let mut registry = SourceRegistry::new();

        registry.register("mock", |config| {
            Ok(Arc::new(MockTestSource {
                id: config.id.clone(),
            }) as Arc<dyn Source>)
        });

        let config = SourceConfig::new("test-source", "mock");
        let source = registry.create(&config).unwrap();
        assert_eq!(source.id(), "test-source");
    }

    #[test]
    fn test_source_registry_unknown_type() {
        let registry = SourceRegistry::new();
        let config = SourceConfig::new("test-source", "unknown");
        match registry.create(&config) {
            Ok(_) => panic!("Expected error for unknown source type"),
            Err(err) => assert!(err.to_string().contains("Unknown source type")),
        }
    }

    #[test]
    fn test_source_registry_has_factory() {
        let mut registry = SourceRegistry::new();
        registry.register("mock", |config| {
            Ok(Arc::new(MockTestSource {
                id: config.id.clone(),
            }) as Arc<dyn Source>)
        });

        assert!(registry.has_factory("mock"));
        assert!(!registry.has_factory("postgres"));
    }
}
