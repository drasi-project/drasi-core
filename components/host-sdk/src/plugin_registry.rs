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
//! This registry supports runtime mutation (registration) and
//! should be wrapped in `Arc<RwLock<PluginRegistry>>` for concurrent access.

use chrono::{DateTime, Utc};
use drasi_plugin_sdk::{
    BootstrapPluginDescriptor, IdentityProviderPluginDescriptor, ReactionPluginDescriptor,
    SourcePluginDescriptor,
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
}

impl<T: ?Sized> std::fmt::Debug for RegisteredDescriptor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredDescriptor")
            .field("plugin_id", &self.plugin_id)
            .field("registered_at", &self.registered_at)
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
/// Plugins are registered at startup and can be dynamically registered
/// at runtime. The registry should be wrapped in `Arc<RwLock<PluginRegistry>>` for
/// thread-safe access.
pub struct PluginRegistry {
    sources: HashMap<String, RegisteredDescriptor<dyn SourcePluginDescriptor>>,
    reactions: HashMap<String, RegisteredDescriptor<dyn ReactionPluginDescriptor>>,
    bootstrappers: HashMap<String, RegisteredDescriptor<dyn BootstrapPluginDescriptor>>,
    identity_providers: HashMap<String, RegisteredDescriptor<dyn IdentityProviderPluginDescriptor>>,
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
            identity_providers: HashMap::new(),
            version: 0,
        }
    }

    /// Current mutation version. Incremented on every register operation.
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
            },
        );
        self.version += 1;
    }

    /// Register a source plugin descriptor with plugin identity metadata.
    pub fn register_source_with_metadata(
        &mut self,
        descriptor: Arc<dyn SourcePluginDescriptor>,
        plugin_id: &str,
    ) {
        let kind = descriptor.kind().to_string();
        self.sources.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
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
            },
        );
        self.version += 1;
    }

    /// Register a reaction plugin descriptor with plugin identity metadata.
    pub fn register_reaction_with_metadata(
        &mut self,
        descriptor: Arc<dyn ReactionPluginDescriptor>,
        plugin_id: &str,
    ) {
        let kind = descriptor.kind().to_string();
        self.reactions.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
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
            },
        );
        self.version += 1;
    }

    /// Register a bootstrap plugin descriptor with plugin identity metadata.
    pub fn register_bootstrapper_with_metadata(
        &mut self,
        descriptor: Arc<dyn BootstrapPluginDescriptor>,
        plugin_id: &str,
    ) {
        let kind = descriptor.kind().to_string();
        self.bootstrappers.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
            },
        );
        self.version += 1;
    }

    /// Register an identity-provider plugin descriptor.
    pub fn register_identity_provider(
        &mut self,
        descriptor: Arc<dyn IdentityProviderPluginDescriptor>,
    ) {
        let kind = descriptor.kind().to_string();
        self.identity_providers.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: String::new(),
                registered_at: Utc::now(),
            },
        );
        self.version += 1;
    }

    /// Register an identity-provider plugin descriptor with plugin identity metadata.
    pub fn register_identity_provider_with_metadata(
        &mut self,
        descriptor: Arc<dyn IdentityProviderPluginDescriptor>,
        plugin_id: &str,
    ) {
        let kind = descriptor.kind().to_string();
        self.identity_providers.insert(
            kind,
            RegisteredDescriptor {
                descriptor,
                plugin_id: plugin_id.to_string(),
                registered_at: Utc::now(),
            },
        );
        self.version += 1;
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

    /// Look up an identity-provider plugin descriptor by kind.
    pub fn get_identity_provider(
        &self,
        kind: &str,
    ) -> Option<&Arc<dyn IdentityProviderPluginDescriptor>> {
        self.identity_providers.get(kind).map(|r| &r.descriptor)
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

    /// Look up an identity-provider registration (descriptor + metadata) by kind.
    pub fn get_identity_provider_registration(
        &self,
        kind: &str,
    ) -> Option<&RegisteredDescriptor<dyn IdentityProviderPluginDescriptor>> {
        self.identity_providers.get(kind)
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

    /// List all registered identity-provider kinds.
    pub fn identity_provider_kinds(&self) -> Vec<&str> {
        let mut kinds: Vec<&str> = self.identity_providers.keys().map(String::as_str).collect();
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

    /// Get detailed info about all registered identity-provider plugins.
    pub fn identity_provider_plugin_infos(&self) -> Vec<PluginKindInfo> {
        let mut infos: Vec<PluginKindInfo> = self
            .identity_providers
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

    /// Returns true if the registry contains no descriptors.
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
            && self.reactions.is_empty()
            && self.bootstrappers.is_empty()
            && self.identity_providers.is_empty()
    }

    /// Returns the total number of registered descriptors.
    pub fn descriptor_count(&self) -> usize {
        self.sources.len()
            + self.reactions.len()
            + self.bootstrappers.len()
            + self.identity_providers.len()
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
            .field("identity_providers", &self.identity_provider_kinds())
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

    struct MockIdentityProviderDescriptor {
        kind: &'static str,
    }

    #[async_trait]
    impl IdentityProviderPluginDescriptor for MockIdentityProviderDescriptor {
        fn kind(&self) -> &str {
            self.kind
        }
        fn config_version(&self) -> &str {
            "1.0.0"
        }
        fn config_schema_json(&self) -> String {
            r#"{"MockIdentityProviderConfig":{"type":"object"}}"#.to_string()
        }
        fn config_schema_name(&self) -> &str {
            "MockIdentityProviderConfig"
        }
        async fn create_identity_provider(
            &self,
            _config_json: &serde_json::Value,
        ) -> anyhow::Result<Box<dyn drasi_lib::identity::IdentityProvider>> {
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
    fn test_source_plugin_infos_includes_plugin_id() {
        let mut registry = PluginRegistry::new();
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "mock" }),
            "drasi-source-mock",
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
    fn test_register_with_metadata_tracks_plugin_id() {
        let mut registry = PluginRegistry::new();

        // Register source with metadata
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "pg" }),
            "drasi-source-pg",
        );
        // Register reaction with metadata
        registry.register_reaction_with_metadata(
            Arc::new(MockReactionDescriptor { kind: "webhook" }),
            "drasi-reaction-webhook",
        );

        // Verify source registration
        let src_reg = registry
            .get_source_registration("pg")
            .expect("source exists");
        assert_eq!(src_reg.plugin_id, "drasi-source-pg");

        // Verify reaction registration
        let rx_reg = registry
            .get_reaction_registration("webhook")
            .expect("reaction exists");
        assert_eq!(rx_reg.plugin_id, "drasi-reaction-webhook");

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
    fn test_descriptor_count_across_categories() {
        let mut registry = PluginRegistry::new();
        assert_eq!(registry.descriptor_count(), 0);

        registry.register_source(Arc::new(MockSourceDescriptor { kind: "s1" }));
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "s2" }));
        registry.register_reaction(Arc::new(MockReactionDescriptor { kind: "r1" }));
        assert_eq!(registry.descriptor_count(), 3);
        assert!(!registry.is_empty());
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
        );

        // Replace same kind with a different plugin_id
        registry.register_source_with_metadata(
            Arc::new(MockSourceDescriptor { kind: "pg" }),
            "plugin-v2",
        );

        let reg = registry.get_source_registration("pg").expect("exists");
        assert_eq!(reg.plugin_id, "plugin-v2");
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
    fn test_register_identity_provider() {
        let mut registry = PluginRegistry::new();
        registry
            .register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "mock" }));

        assert_eq!(registry.identity_provider_kinds(), vec!["mock"]);
        assert!(registry.get_identity_provider("mock").is_some());
        assert!(registry.get_identity_provider("nonexistent").is_none());
        assert_eq!(registry.descriptor_count(), 1);
        assert!(!registry.is_empty());
        assert_eq!(registry.version(), 1);
    }

    #[test]
    fn test_register_identity_provider_with_metadata_tracks_plugin_id() {
        let mut registry = PluginRegistry::new();
        registry.register_identity_provider_with_metadata(
            Arc::new(MockIdentityProviderDescriptor { kind: "azure" }),
            "drasi-identity-azure",
        );

        let reg = registry
            .get_identity_provider_registration("azure")
            .expect("identity provider exists");
        assert_eq!(reg.plugin_id, "drasi-identity-azure");

        let infos = registry.identity_provider_plugin_infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].plugin_id, "drasi-identity-azure");
        assert_eq!(infos[0].kind, "azure");
        assert_eq!(infos[0].config_version, "1.0.0");
        assert_eq!(infos[0].config_schema_name, "MockIdentityProviderConfig");
    }

    #[test]
    fn test_identity_provider_version_increments() {
        let mut registry = PluginRegistry::new();
        let v0 = registry.version();

        registry.register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "a" }));
        assert_eq!(registry.version(), v0 + 1);

        registry.register_identity_provider_with_metadata(
            Arc::new(MockIdentityProviderDescriptor { kind: "b" }),
            "plugin-b",
        );
        assert_eq!(registry.version(), v0 + 2);
    }

    #[test]
    fn test_identity_provider_replace_updates_metadata() {
        let mut registry = PluginRegistry::new();
        registry.register_identity_provider_with_metadata(
            Arc::new(MockIdentityProviderDescriptor { kind: "azure" }),
            "plugin-v1",
        );
        registry.register_identity_provider_with_metadata(
            Arc::new(MockIdentityProviderDescriptor { kind: "azure" }),
            "plugin-v2",
        );

        let reg = registry
            .get_identity_provider_registration("azure")
            .expect("exists");
        assert_eq!(reg.plugin_id, "plugin-v2");
        assert_eq!(registry.descriptor_count(), 1);
    }

    #[test]
    fn test_identity_provider_kinds_are_sorted() {
        let mut registry = PluginRegistry::new();
        registry
            .register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "zeta" }));
        registry
            .register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "alpha" }));
        registry
            .register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "beta" }));

        assert_eq!(
            registry.identity_provider_kinds(),
            vec!["alpha", "beta", "zeta"]
        );
    }

    #[test]
    fn test_descriptor_count_includes_identity_providers() {
        let mut registry = PluginRegistry::new();
        registry.register_source(Arc::new(MockSourceDescriptor { kind: "s1" }));
        registry.register_reaction(Arc::new(MockReactionDescriptor { kind: "r1" }));
        registry
            .register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "i1" }));
        registry
            .register_identity_provider(Arc::new(MockIdentityProviderDescriptor { kind: "i2" }));

        assert_eq!(registry.descriptor_count(), 4);
        assert!(!registry.is_empty());
    }

    #[test]
    fn test_get_identity_provider_registration_none_for_missing() {
        let registry = PluginRegistry::new();
        assert!(registry.get_identity_provider("missing").is_none());
        assert!(registry
            .get_identity_provider_registration("missing")
            .is_none());
        assert!(registry.identity_provider_plugin_infos().is_empty());
    }
}
