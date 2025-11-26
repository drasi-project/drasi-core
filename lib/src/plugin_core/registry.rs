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

//! Plugin registry for source and reaction factories
//!
//! This module provides a pluggable registry system that allows sources and reactions
//! to be registered and instantiated dynamically without hard-coded match statements.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

use crate::channels::ComponentEventSender;
use crate::config::{ReactionConfig, SourceConfig};

// Use the actual Source and Reaction traits from the modules
// These are the public-facing traits used by managers
use crate::sources::Source as SourceTrait;
use crate::reactions::Reaction as ReactionTrait;

/// Factory function signature for creating sources
pub type SourceFactoryFn =
    Box<dyn Fn(SourceConfig, ComponentEventSender) -> Result<Arc<dyn SourceTrait>> + Send + Sync>;

/// Factory function signature for creating reactions
pub type ReactionFactoryFn =
    Box<dyn Fn(ReactionConfig, ComponentEventSender) -> Result<Arc<dyn ReactionTrait>> + Send + Sync>;

/// Registry for source factories
pub struct SourceRegistry {
    factories: HashMap<String, SourceFactoryFn>,
}

impl SourceRegistry {
    /// Create a new source registry
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Create the default registry with built-in sources
    pub fn default() -> Self {
        let mut registry = Self::new();
        registry.register_defaults();
        registry
    }

    /// Register a source factory
    pub fn register<F>(&mut self, source_type: String, factory: F)
    where
        F: Fn(SourceConfig, ComponentEventSender) -> Result<Arc<dyn SourceTrait>> + Send + Sync + 'static,
    {
        self.factories.insert(source_type, Box::new(factory));
    }

    /// Register all default/built-in sources
    fn register_defaults(&mut self) {
        // No built-in source implementations - all sources are provided by plugins
    }

    /// Create a source instance from configuration
    pub fn create(
        &self,
        config: SourceConfig,
        event_tx: ComponentEventSender,
    ) -> Result<Arc<dyn SourceTrait>> {
        let source_type = config.source_type();

        self.factories
            .get(source_type)
            .ok_or_else(|| anyhow::anyhow!("Unknown source type: {}", source_type))?
            (config, event_tx)
    }

    /// Check if a source type is registered
    pub fn is_registered(&self, source_type: &str) -> bool {
        self.factories.contains_key(source_type)
    }

    /// Get list of registered source types
    pub fn registered_types(&self) -> Vec<String> {
        self.factories.keys().cloned().collect()
    }
}

impl Default for SourceRegistry {
    fn default() -> Self {
        Self::default()
    }
}

/// Registry for reaction factories
pub struct ReactionRegistry {
    factories: HashMap<String, ReactionFactoryFn>,
}

impl ReactionRegistry {
    /// Create a new reaction registry
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Create the default registry with built-in reactions
    pub fn default() -> Self {
        let mut registry = Self::new();
        registry.register_defaults();
        registry
    }

    /// Register a reaction factory
    pub fn register<F>(&mut self, reaction_type: String, factory: F)
    where
        F: Fn(ReactionConfig, ComponentEventSender) -> Result<Arc<dyn ReactionTrait>>
            + Send
            + Sync
            + 'static,
    {
        self.factories.insert(reaction_type, Box::new(factory));
    }

    /// Register all default/built-in reactions
    fn register_defaults(&mut self) {
        // No built-in reaction implementations - all reactions are provided by plugins
    }

    /// Create a reaction instance from configuration
    pub fn create(
        &self,
        config: ReactionConfig,
        event_tx: ComponentEventSender,
    ) -> Result<Arc<dyn ReactionTrait>> {
        let reaction_type = config.reaction_type();

        self.factories
            .get(reaction_type)
            .ok_or_else(|| anyhow::anyhow!("Unknown reaction type: {}", reaction_type))?
            (config, event_tx)
    }

    /// Check if a reaction type is registered
    pub fn is_registered(&self, reaction_type: &str) -> bool {
        self.factories.contains_key(reaction_type)
    }

    /// Get list of registered reaction types
    pub fn registered_types(&self) -> Vec<String> {
        self.factories.keys().cloned().collect()
    }
}

impl Default for ReactionRegistry {
    fn default() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_registry_has_defaults() {
        let registry = SourceRegistry::default();
        assert_eq!(registry.registered_types().len(), 0);
    }

    #[test]
    fn test_reaction_registry_has_defaults() {
        let registry = ReactionRegistry::default();
        assert_eq!(registry.registered_types().len(), 0);
    }

    #[test]
    fn test_registered_types() {
        let registry = SourceRegistry::default();
        let types = registry.registered_types();
        assert_eq!(types.len(), 0);
    }
}
