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

//! Plugin registry for managing dynamically-registered plugin descriptors.
//!
//! The [`PluginRegistry`] holds all known plugin descriptors (sources, reactions,
//! bootstrappers) and provides lookup, creation, and schema introspection methods.
//!
//! This registry supports runtime mutation (registration and deregistration) and
//! should be wrapped in `Arc<RwLock<PluginRegistry>>` for concurrent access.

use chrono::{DateTime, Utc};
use drasi_plugin_sdk::{
    BootstrapPluginDescriptor, ReactionPluginDescriptor, SourcePluginDescriptor,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Wrapper around a descriptor that tracks which plugin provided it.
pub struct RegisteredDescriptor<T: ?Sized> {
    /// The actual descriptor.
    pub descriptor: Arc<T>,
    /// ID of the plugin that provided this descriptor.
    pub plugin_id: String,
    /// Timestamp when this descriptor was registered.
    pub registered_at: DateTime<Utc>,
    /// Library generation counter for drain-then-replace.
    pub generation: u64,
}

impl<T: ?Sized> std::fmt::Debug for RegisteredDescriptor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredDescriptor")
            .field("plugin_id", &self.plugin_id)
            .field("registered_at", &self.registered_at)
            .field("generation", &self.generation)
            .finish()
    }
}

/// Information about a registered plugin kind.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginKindInfo {
    /// The plugin kind identifier.
    pub kind: String,
    /// The semver version of the plugin's configuration DTO.
    pub config_version: String,
    /// The plugin's configuration schema as a JSON string.
    pub config_schema_json: String,
    /// The OpenAPI schema name for this plugin's config DTO.
    pub config_schema_name: String,
    /// ID of the plugin that provides this kind (empty for core plugins).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub plugin_id: String,
}

/// Registry of all known plugin descriptors.
///
/// Plugins are registered at startup and can be dynamically registered/deregistered
/// at runtime. The registry should be wrapped in `Arc<RwLock<PluginRegistry>>` for
/// thread-safe access.
pub struct PluginRegistry {
    sources: HashMap<String, RegisteredDescriptor<dyn SourcePluginDescriptor>>,
    reactions: HashMap<String, RegisteredDescriptor<dyn ReactionPluginDescriptor>>,
    bootstrappers: HashMap<String, RegisteredDescriptor<dyn BootstrapPluginDescriptor>>,
    /// Monotonically increasing counter incremented on every mutation.
    /// Used by OpenAPI cache invalidation and other version-sensitive consumers.
    version: u64,
}

impl PluginRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            reactions: HashMap::new(),
            bootstrappers: HashMap::new(),
            version: 0,
        }
    }

    /// Current mutation version. Incremented on every register/deregister operation.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Register a source plugin descriptor.
    ///
    /// If a source with the same kind is already registered, it is replaced.
    pub fn register_source(&mut self, descriptor: Arc<dyn SourcePluginDescriptor>) {
        let kind = descriptor.kind().to_string();
        self.sources.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: String::new(),
                registered_at: Utc::now(),
                generation: 0,
            },
        );
        self.version += 1;
    }

    /// Register a source plugin descriptor with plugin identity metadata.
    pub fn register_source_with_metadata(
        &mut self,
        descriptor: Arc<dyn SourcePluginDescriptor>,
        plugin_id: &str,
        generation: u64,
    ) {
        let kind = descriptor.kind().to_string();
        self.sources.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
                generation,
            },
        );
        self.version += 1;
    }

    /// Register a reaction plugin descriptor.
    pub fn register_reaction(&mut self, descriptor: Arc<dyn ReactionPluginDescriptor>) {
        let kind = descriptor.kind().to_string();
        self.reactions.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: String::new(),
                registered_at: Utc::now(),
                generation: 0,
            },
        );
        self.version += 1;
    }

    /// Register a reaction plugin descriptor with plugin identity metadata.
    pub fn register_reaction_with_metadata(
        &mut self,
        descriptor: Arc<dyn ReactionPluginDescriptor>,
        plugin_id: &str,
        generation: u64,
    ) {
        let kind = descriptor.kind().to_string();
        self.reactions.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
                generation,
            },
        );
        self.version += 1;
    }

    /// Register a bootstrap plugin descriptor.
    pub fn register_bootstrapper(&mut self, descriptor: Arc<dyn BootstrapPluginDescriptor>) {
        let kind = descriptor.kind().to_string();
        self.bootstrappers.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: String::new(),
                registered_at: Utc::now(),
                generation: 0,
            },
        );
        self.version += 1;
    }

    /// Register a bootstrap plugin descriptor with plugin identity metadata.
    pub fn register_bootstrapper_with_metadata(
        &mut self,
        descriptor: Arc<dyn BootstrapPluginDescriptor>,
        plugin_id: &str,
        generation: u64,
    ) {
        let kind = descriptor.kind().to_string();
        self.bootstrappers.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
                generation,
            },
        );
        self.version += 1;
    }

    /// Deregister a source descriptor by kind. Returns true if it existed.
    pub fn deregister_source(&mut self, kind: &str) -> bool {
        let removed = self.sources.remove(kind).is_some();
        if removed {
            self.version += 1;
        }
        removed
    }

    /// Deregister a reaction descriptor by kind. Returns true if it existed.
    pub fn deregister_reaction(&mut self, kind: &str) -> bool {
        let removed = self.reactions.remove(kind).is_some();
        if removed {
            self.version += 1;
        }
        removed
    }

    /// Deregister a bootstrap descriptor by kind. Returns true if it existed.
    pub fn deregister_bootstrapper(&mut self, kind: &str) -> bool {
        let removed = self.bootstrappers.remove(kind).is_some();
        if removed {
            self.version += 1;
        }
        removed
    }

    /// Deregister all descriptors provided by a specific plugin.
    /// Returns the number of descriptors removed.
    pub fn deregister_all_for_plugin(&mut self, plugin_id: &str) -> usize {
        let before = self.descriptor_count();

        self.sources
            .retain(|_, v| v.plugin_id != plugin_id || plugin_id.is_empty());
        self.reactions
            .retain(|_, v| v.plugin_id != plugin_id || plugin_id.is_empty());
        self.bootstrappers
            .retain(|_, v| v.plugin_id != plugin_id || plugin_id.is_empty());

        let removed = before - self.descriptor_count();
        if removed > 0 {
            self.version += 1;
        }
        removed
    }

    /// Look up a source plugin descriptor by kind.
    pub fn get_source(&self, kind: &str) -> Option<&Arc<dyn SourcePluginDescriptor>> {
        self.sources.get(kind).map(|r| &r.descriptor)
    }

    /// Look up a reaction plugin descriptor by kind.
    pub fn get_reaction(&self, kind: &str) -> Option<&Arc<dyn ReactionPluginDescriptor>> {
        self.reactions.get(kind).map(|r| &r.descriptor)
    }

    /// Look up a bootstrap plugin descriptor by kind.
    pub fn get_bootstrapper(&self, kind: &str) -> Option<&Arc<dyn BootstrapPluginDescriptor>> {
        self.bootstrappers.get(kind).map(|r| &r.descriptor)
    }

    /// Look up a source registration (descriptor + metadata) by kind.
    pub fn get_source_registration(
        &self,
        kind: &str,
    ) -> Option<&RegisteredDescriptor<dyn SourcePluginDescriptor>> {
        self.sources.get(kind)
    }

    /// Look up a reaction registration (descriptor + metadata) by kind.
    pub fn get_reaction_registration(
        &self,
        kind: &str,
    ) -> Option<&RegisteredDescriptor<dyn ReactionPluginDescriptor>> {
        self.reactions.get(kind)
    }

    /// Look up a bootstrap registration (descriptor + metadata) by kind.
    pub fn get_bootstrapper_registration(
        &self,
        kind: &str,
    ) -> Option<&RegisteredDescriptor<dyn BootstrapPluginDescriptor>> {
        self.bootstrappers.get(kind)
    }

    /// List all registered source kinds.
    pub fn source_kinds(&self) -> Vec<&str> {
        let mut kinds: Vec<&str> = self.sources.keys().map(String::as_str).collect();
        kinds.sort();
        kinds
    }

    /// List all registered reaction kinds.
    pub fn reaction_kinds(&self) -> Vec<&str> {
        let mut kinds: Vec<&str> = self.reactions.keys().map(String::as_str).collect();
        kinds.sort();
        kinds
    }

    /// List all registered bootstrapper kinds.
    pub fn bootstrapper_kinds(&self) -> Vec<&str> {
        let mut kinds: Vec<&str> = self.bootstrappers.keys().map(String::as_str).collect();
        kinds.sort();
        kinds
    }

    /// Get detailed info about all registered source plugins.
    pub fn source_plugin_infos(&self) -> Vec<PluginKindInfo> {
        let mut infos: Vec<PluginKindInfo> = self
            .sources
            .values()
            .map(|r| PluginKindInfo {
                kind: r.descriptor.kind().to_string(),
                config_version: r.descriptor.config_version().to_string(),
                config_schema_json: r.descriptor.config_schema_json(),
                config_schema_name: r.descriptor.config_schema_name().to_string(),
                plugin_id: r.plugin_id.clone(),
            })
            .collect();
        infos.sort_by(|a, b| a.kind.cmp(&b.kind));
        infos
    }

    /// Get detailed info about all registered reaction plugins.
    pub fn reaction_plugin_infos(&self) -> Vec<PluginKindInfo> {
        let mut infos: Vec<PluginKindInfo> = self
            .reactions
            .values()
            .map(|r| PluginKindInfo {
                kind: r.descriptor.kind().to_string(),
                config_version: r.descriptor.config_version().to_string(),
                config_schema_json: r.descriptor.config_schema_json(),
                config_schema_name: r.descriptor.config_schema_name().to_string(),
                plugin_id: r.plugin_id.clone(),
            })
            .collect();
        infos.sort_by(|a, b| a.kind.cmp(&b.kind));
        infos
    }

    /// Get detailed info about all registered bootstrap plugins.
    pub fn bootstrapper_plugin_infos(&self) -> Vec<PluginKindInfo> {
        let mut infos: Vec<PluginKindInfo> = self
            .bootstrappers
            .values()
            .map(|r| PluginKindInfo {
                kind: r.descriptor.kind().to_string(),
                config_version: r.descriptor.config_version().to_string(),
                config_schema_json: r.descriptor.config_schema_json(),
                config_schema_name: r.descriptor.config_schema_name().to_string(),
                plugin_id: r.plugin_id.clone(),
            })
            .collect();
        infos.sort_by(|a, b| a.kind.cmp(&b.kind));
        infos
    }

    // ── Side-by-side versioned registration and promotion ──

    /// Register a source descriptor under a versioned kind key (e.g., "postgres@0.4.2").
    /// Does not affect the unversioned key.
    pub fn register_source_versioned(
        &mut self,
        versioned_key: &str,
        descriptor: Arc<dyn SourcePluginDescriptor>,
        plugin_id: &str,
        generation: u64,
    ) {
        self.sources.insert(
            versioned_key.to_string(),
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
                generation,
            },
        );
        self.version += 1;
    }

    /// Promote a versioned source descriptor to be the incumbent (unversioned key).
    /// Returns true if successful, false if the versioned key doesn't exist.
    pub fn promote_source(&mut self, versioned_key: &str) -> bool {
        if let Some(reg) = self.sources.get(versioned_key) {
            let kind = reg.descriptor.kind().to_string();
            let promoted = RegisteredDescriptor {
                descriptor: reg.descriptor.clone(),
                plugin_id: reg.plugin_id.clone(),
                registered_at: Utc::now(),
                generation: reg.generation,
            };
            self.sources.insert(kind, promoted);
            self.version += 1;
            true
        } else {
            false
        }
    }

    /// Register a reaction descriptor under a versioned kind key (e.g., "log@0.4.2").
    /// Does not affect the unversioned key.
    pub fn register_reaction_versioned(
        &mut self,
        versioned_key: &str,
        descriptor: Arc<dyn ReactionPluginDescriptor>,
        plugin_id: &str,
        generation: u64,
    ) {
        self.reactions.insert(
            versioned_key.to_string(),
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
                generation,
            },
        );
        self.version += 1;
    }

    /// Promote a versioned reaction descriptor to be the incumbent (unversioned key).
    /// Returns true if successful, false if the versioned key doesn't exist.
    pub fn promote_reaction(&mut self, versioned_key: &str) -> bool {
        if let Some(reg) = self.reactions.get(versioned_key) {
            let kind = reg.descriptor.kind().to_string();
            let promoted = RegisteredDescriptor {
                descriptor: reg.descriptor.clone(),
                plugin_id: reg.plugin_id.clone(),
                registered_at: Utc::now(),
                generation: reg.generation,
            };
            self.reactions.insert(kind, promoted);
            self.version += 1;
            true
        } else {
            false
        }
    }

    /// Register a bootstrap descriptor under a versioned kind key.
    /// Does not affect the unversioned key.
    pub fn register_bootstrapper_versioned(
        &mut self,
        versioned_key: &str,
        descriptor: Arc<dyn BootstrapPluginDescriptor>,
        plugin_id: &str,
        generation: u64,
    ) {
        self.bootstrappers.insert(
            versioned_key.to_string(),
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
                generation,
            },
        );
        self.version += 1;
    }

    /// Promote a versioned bootstrapper descriptor to be the incumbent (unversioned key).
    /// Returns true if successful, false if the versioned key doesn't exist.
    pub fn promote_bootstrapper(&mut self, versioned_key: &str) -> bool {
        if let Some(reg) = self.bootstrappers.get(versioned_key) {
            let kind = reg.descriptor.kind().to_string();
            let promoted = RegisteredDescriptor {
                descriptor: reg.descriptor.clone(),
                plugin_id: reg.plugin_id.clone(),
                registered_at: Utc::now(),
                generation: reg.generation,
            };
            self.bootstrappers.insert(kind, promoted);
            self.version += 1;
            true
        } else {
            false
        }
    }

    /// Returns true if the registry contains no descriptors.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty() && self.reactions.is_empty() && self.bootstrappers.is_empty()
    }

    /// Returns the total number of registered descriptors.
    pub fn descriptor_count(&self) -> usize {
        self.sources.len() + self.reactions.len() + self.bootstrappers.len()
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for PluginRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginRegistry")
            .field("sources", &self.source_kinds())
            .field("reactions", &self.reaction_kinds())
            .field("bootstrappers", &self.bootstrapper_kinds())
            .field("version", &self.version)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use drasi_lib::sources::Source;

    struct MockSourceDescriptor {
        kind: &'static str,
    }

    #[async_trait]
    impl SourcePluginDescriptor for MockSourceDescriptor {
        fn kind(&self) -> &str {
            self.kind
        }
        fn config_version(&self) -> &str {
            "1.0.0"
        }
        fn config_schema_json(&self) -> String {
            r#"{"MockSourceConfig":{"type":"object","properties":{"host":{"type":"string"}}}}"#
                .to_string()
        }
        fn config_schema_name(&self) -> &str {
            "MockSourceConfig"
        }
        async fn create_source(
            &self,
            _id: &str,
            _config_json: &serde_json::Value,
            _auto_start: bool,
        ) -> anyhow::Result<Box<dyn Source>> {
            anyhow::bail!("mock: not implemented")
        }
    }

    struct MockReactionDescriptor {
        kind: &'static str,
    }

    #[async_trait]
    impl ReactionPluginDescriptor for MockReactionDescriptor {
        fn kind(&self) -> &str {
            self.kind
        }
        fn config_version(&self) -> &str {
            "1.0.0"
        }
        fn config_schema_json(&self) -> String {
            r#"{"MockReactionConfig":{"type":"object"}}"#.to_string()
        }
        fn config_schema_name(&self) -> &str {
            "MockReactionConfig"
        }
        async fn create_reaction(
            &self,
            _id: &str,
            _query_ids: Vec<String>,
            _config_json: &serde_json::Value,
            _auto_start: bool,
        ) -> anyhow::Result<Box<dyn drasi_lib::reactions::Reaction>> {
            anyhow::bail!("mock: not implemented")
        }
    }

    #[test]
    fn test_new_registry_is_empty() {
        let registry = PluginRegistry::new();
        assert!(registry.is_empty());
        assert_eq!(registry.descriptor_count(), 0);
        assert_eq!(registry.version(), 0);
    }

    #[test]
    fn test_register_source() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "mock" }));

        assert_eq!(registry.source_kinds(), vec!["mock"]);
        assert!(registry.get_source("mock").is_some());
        assert!(registry.get_source("nonexistent").is_none());
        assert_eq!(registry.descriptor_count(), 1);
        assert_eq!(registry.version(), 1);
    }

    #[test]
    fn test_register_with_metadata() {
        let mut registry = PluginRegistry::new();
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "mock" }),
            "drasi-source-mock",
            42,
        );

        let reg = registry.get_source_registration("mock").expect("exists");
        assert_eq!(reg.plugin_id, "drasi-source-mock");
        assert_eq!(reg.generation, 42);
    }

    #[test]
    fn test_deregister_source() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "mock" }));
        assert_eq!(registry.version(), 1);

        let removed = registry.deregister_source("mock");
        assert!(removed);
        assert!(registry.get_source("mock").is_none());
        assert_eq!(registry.version(), 2);

        let removed_again = registry.deregister_source("mock");
        assert!(!removed_again);
        assert_eq!(registry.version(), 2); // no change
    }

    #[test]
    fn test_deregister_all_for_plugin() {
        let mut registry = PluginRegistry::new();
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "postgres" }),
            "drasi-source-postgres",
            1,
        );
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "mock" }),
            "drasi-source-mock",
            1,
        );
        registry.register_reaction_with_metadata(
            Arc::new(MockReactionDescriptor { kind: "log" }),
            "drasi-source-postgres", // hypothetical multi-descriptor plugin
            1,
        );

        let removed = registry.deregister_all_for_plugin("drasi-source-postgres");
        assert_eq!(removed, 2); // postgres source + log reaction
        assert!(registry.get_source("postgres").is_none());
        assert!(registry.get_reaction("log").is_none());
        assert!(registry.get_source("mock").is_some()); // different plugin
    }

    #[test]
    fn test_source_plugin_infos_includes_plugin_id() {
        let mut registry = PluginRegistry::new();
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "mock" }),
            "drasi-source-mock",
            1,
        );

        let infos = registry.source_plugin_infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].plugin_id, "drasi-source-mock");
    }

    #[test]
    fn test_version_increments_on_mutations() {
        let mut registry = PluginRegistry::new();
        assert_eq!(registry.version(), 0);

        registry.register_source(Arc::new(MockSourceDescriptor { kind: "a" }));
        assert_eq!(registry.version(), 1);

        registry.register_reaction(Arc::new(MockReactionDescriptor { kind: "b" }));
        assert_eq!(registry.version(), 2);

        registry.deregister_source("a");
        assert_eq!(registry.version(), 3);
    }

    #[test]
    fn test_duplicate_kind_replaces() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "mock" }));
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "mock" }));

        assert_eq!(registry.source_kinds(), vec!["mock"]);
        assert_eq!(registry.descriptor_count(), 1);
    }

    #[test]
    fn test_kinds_are_sorted() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "zeta" }));
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "alpha" }));
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "beta" }));

        assert_eq!(registry.source_kinds(), vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn test_deregister_reaction() {
        let mut registry = PluginRegistry::new();
        registry.register_reaction(Arc::new(MockReactionDescriptor { kind: "log" }));
        assert_eq!(registry.reaction_kinds(), vec!["log"]);
        assert_eq!(registry.version(), 1);

        let removed = registry.deregister_reaction("log");
        assert!(removed);
        assert!(registry.get_reaction("log").is_none());
        assert!(registry.reaction_kinds().is_empty());
        assert_eq!(registry.version(), 2);
    }

    #[test]
    fn test_deregister_nonexistent_returns_false() {
        let mut registry = PluginRegistry::new();

        assert!(!registry.deregister_source("nope"));
        assert!(!registry.deregister_reaction("nope"));
        assert!(!registry.deregister_bootstrapper("nope"));
    }

    #[test]
    fn test_version_not_incremented_on_noop_deregister() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "a" }));
        let v = registry.version();

        // Deregistering a nonexistent kind should not bump the version
        registry.deregister_source("nonexistent");
        assert_eq!(registry.version(), v);

        registry.deregister_reaction("nonexistent");
        assert_eq!(registry.version(), v);

        registry.deregister_bootstrapper("nonexistent");
        assert_eq!(registry.version(), v);

        // deregister_all_for_plugin with an unknown plugin_id also should not bump
        let removed = registry.deregister_all_for_plugin("unknown-plugin");
        assert_eq!(removed, 0);
        assert_eq!(registry.version(), v);
    }

    #[test]
    fn test_register_with_metadata_tracks_plugin_id() {
        let mut registry = PluginRegistry::new();

        // Register source with metadata
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "pg" }),
            "drasi-source-pg",
            10,
        );
        // Register reaction with metadata
        registry.register_reaction_with_metadata(
            Arc::new(MockReactionDescriptor { kind: "webhook" }),
            "drasi-reaction-webhook",
            20,
        );

        // Verify source registration
        let src_reg = registry
            .get_source_registration("pg")
            .expect("source exists");
        assert_eq!(src_reg.plugin_id, "drasi-source-pg");
        assert_eq!(src_reg.generation, 10);

        // Verify reaction registration
        let rx_reg = registry
            .get_reaction_registration("webhook")
            .expect("reaction exists");
        assert_eq!(rx_reg.plugin_id, "drasi-reaction-webhook");
        assert_eq!(rx_reg.generation, 20);

        // Verify plugin_id propagates to plugin_infos
        let src_infos = registry.source_plugin_infos();
        assert_eq!(src_infos.len(), 1);
        assert_eq!(src_infos[0].plugin_id, "drasi-source-pg");
        assert_eq!(src_infos[0].kind, "pg");

        let rx_infos = registry.reaction_plugin_infos();
        assert_eq!(rx_infos.len(), 1);
        assert_eq!(rx_infos[0].plugin_id, "drasi-reaction-webhook");
        assert_eq!(rx_infos[0].kind, "webhook");
    }

    #[test]
    fn test_deregister_all_for_plugin_ignores_empty_plugin_id() {
        let mut registry = PluginRegistry::new();
        // register_source (without metadata) sets plugin_id to ""
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "core_src" }));
        registry.register_reaction(Arc::new(MockReactionDescriptor { kind: "core_rx" }));

        // Attempting to deregister_all_for_plugin("") should NOT remove
        // descriptors with empty plugin_id (per the retain guard).
        let removed = registry.deregister_all_for_plugin("");
        assert_eq!(removed, 0);
        assert!(registry.get_source("core_src").is_some());
        assert!(registry.get_reaction("core_rx").is_some());
    }

    #[test]
    fn test_descriptor_count_across_categories() {
        let mut registry = PluginRegistry::new();
        assert_eq!(registry.descriptor_count(), 0);

        registry.register_source(Arc::new(MockSourceDescriptor { kind: "s1" }));
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "s2" }));
        registry.register_reaction(Arc::new(MockReactionDescriptor { kind: "r1" }));
        assert_eq!(registry.descriptor_count(), 3);
        assert!(!registry.is_empty());

        registry.deregister_source("s1");
        assert_eq!(registry.descriptor_count(), 2);
    }

    #[test]
    fn test_get_registration_none_for_missing() {
        let registry = PluginRegistry::new();
        assert!(registry.get_source_registration("x").is_none());
        assert!(registry.get_reaction_registration("x").is_none());
        assert!(registry.get_bootstrapper_registration("x").is_none());
    }

    #[test]
    fn test_replace_updates_metadata() {
        let mut registry = PluginRegistry::new();
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "pg" }),
            "plugin-v1",
            1,
        );

        // Replace same kind with a different plugin_id and generation
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "pg" }),
            "plugin-v2",
            5,
        );

        let reg = registry.get_source_registration("pg").expect("exists");
        assert_eq!(reg.plugin_id, "plugin-v2");
        assert_eq!(reg.generation, 5);
        assert_eq!(registry.descriptor_count(), 1); // still just one
    }

    #[test]
    fn test_default_trait() {
        let registry = PluginRegistry::default();
        assert!(registry.is_empty());
        assert_eq!(registry.version(), 0);
    }

    #[test]
    fn test_debug_format() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "mock" }));
        let debug_str = format!("{registry:?}");
        assert!(debug_str.contains("PluginRegistry"));
        assert!(debug_str.contains("mock"));
    }

    #[test]
    fn test_register_source_versioned_does_not_affect_unversioned() {
        let mut registry = PluginRegistry::new();
        // Register the incumbent under the unversioned key
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "postgres" }),
            "plugin-v1",
            1,
        );
        let v_before = registry.version();

        // Register a new version under a versioned key
        registry.register_source_versioned(
            "postgres@0.4.2",
            Arc::new(MockSourceDescriptor { kind: "postgres" }),
            "plugin-v2",
            2,
        );

        // The unversioned key should still point to the old plugin
        let incumbent = registry
            .get_source_registration("postgres")
            .expect("exists");
        assert_eq!(incumbent.plugin_id, "plugin-v1");

        // The versioned key should exist separately
        let versioned = registry
            .get_source_registration("postgres@0.4.2")
            .expect("exists");
        assert_eq!(versioned.plugin_id, "plugin-v2");

        // Both keys count as separate descriptors
        assert!(registry.version() > v_before);
    }

    #[test]
    fn test_promote_source_updates_unversioned_key() {
        let mut registry = PluginRegistry::new();
        // Register incumbent
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "postgres" }),
            "plugin-v1",
            1,
        );
        // Register versioned
        registry.register_source_versioned(
            "postgres@0.4.2",
            Arc::new(MockSourceDescriptor { kind: "postgres" }),
            "plugin-v2",
            2,
        );

        // Promote
        let promoted = registry.promote_source("postgres@0.4.2");
        assert!(promoted);

        // Unversioned key should now point to plugin-v2
        let incumbent = registry
            .get_source_registration("postgres")
            .expect("exists");
        assert_eq!(incumbent.plugin_id, "plugin-v2");
        assert_eq!(incumbent.generation, 2);
    }

    #[test]
    fn test_promote_source_nonexistent_returns_false() {
        let mut registry = PluginRegistry::new();
        let promoted = registry.promote_source("nonexistent@1.0.0");
        assert!(!promoted);
    }

    #[test]
    fn test_promote_reaction() {
        let mut registry = PluginRegistry::new();
        registry.register_reaction_with_metadata(
            Arc::new(MockReactionDescriptor { kind: "log" }),
            "plugin-v1",
            1,
        );
        registry.register_reaction_versioned(
            "log@2.0.0",
            Arc::new(MockReactionDescriptor { kind: "log" }),
            "plugin-v2",
            2,
        );

        let promoted = registry.promote_reaction("log@2.0.0");
        assert!(promoted);

        let incumbent = registry.get_reaction_registration("log").expect("exists");
        assert_eq!(incumbent.plugin_id, "plugin-v2");
    }

    #[test]
    fn test_promote_reaction_nonexistent_returns_false() {
        let mut registry = PluginRegistry::new();
        assert!(!registry.promote_reaction("nope@1.0"));
    }

    #[test]
    fn test_promote_bootstrapper_nonexistent_returns_false() {
        let mut registry = PluginRegistry::new();
        assert!(!registry.promote_bootstrapper("nope@1.0"));
    }
}
