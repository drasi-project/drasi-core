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

//! Plugin lifecycle management — reusable runtime lifecycle for all Drasi hosts.
//!
//! The [`PluginLifecycleManager`] owns the mutable [`PluginRegistry`] and provides
//! operations to load plugins at runtime. It emits [`PluginEvent`]s
//! through a broadcast channel so host applications can react to plugin changes.
//!
//! This lives in `host-sdk` so it is available to any Drasi host implementation,
//! not just `drasi-server`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

use crate::callbacks::{self, CallbackContext};
use crate::loader::{load_plugin_from_path, plugin_kind_from_filename, LoadedPlugin};
use crate::plugin_registry::PluginRegistry;
use crate::plugin_types::{PluginCategory, PluginEvent, PluginKindEntry, PluginStatus};

use drasi_plugin_sdk::{
    BootstrapPluginDescriptor, IdentityProviderPluginDescriptor, ReactionPluginDescriptor,
    SourcePluginDescriptor,
};

/// Tracks the runtime state of a single loaded plugin library.
#[derive(Debug)]
pub struct LoadedPluginState {
    pub plugin_id: String,
    pub status: PluginStatus,
    pub kinds: Vec<PluginKindEntry>,
    pub metadata_info: Option<String>,
}

/// Manages plugin loading and registration at the host-sdk level.
///
/// The `PluginLifecycleManager` is the reusable core that any Drasi host can use
/// to manage plugin lifecycles. It owns the `PluginRegistry` (via `Arc<RwLock>`)
/// and emits `PluginEvent`s through a broadcast channel.
pub struct PluginLifecycleManager {
    registry: Arc<RwLock<PluginRegistry>>,
    loaded_plugins: RwLock<HashMap<String, LoadedPluginState>>,
    event_tx: broadcast::Sender<PluginEvent>,
}

impl PluginLifecycleManager {
    /// Create a new lifecycle manager with the given registry.
    pub fn new(registry: Arc<RwLock<PluginRegistry>>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            registry,
            loaded_plugins: RwLock::new(HashMap::new()),
            event_tx,
        }
    }

    /// Get a reference to the shared plugin registry.
    pub fn registry(&self) -> &Arc<RwLock<PluginRegistry>> {
        &self.registry
    }

    /// Subscribe to plugin lifecycle events.
    pub fn subscribe(&self) -> broadcast::Receiver<PluginEvent> {
        self.event_tx.subscribe()
    }

    /// Register descriptors from an already-loaded plugin into the registry.
    ///
    /// This takes ownership of the `LoadedPlugin`, wraps each descriptor proxy
    /// in `Arc`, and registers them with plugin identity metadata. Use this when
    /// the caller has already called `load_plugin_from_path` and wants to register
    /// the results.
    pub async fn register_loaded_plugin(
        &self,
        plugin_id: &str,
        mut loaded: LoadedPlugin,
    ) -> Vec<PluginKindEntry> {
        let mut kinds = Vec::new();
        let metadata_info = loaded.metadata_info.take();

        // Take ownership of proxy vecs via mem::take, so Drop finds them empty.
        let sources = std::mem::take(&mut loaded.source_plugins);
        let reactions = std::mem::take(&mut loaded.reaction_plugins);
        let bootstraps = std::mem::take(&mut loaded.bootstrap_plugins);
        let identity_providers = std::mem::take(&mut loaded.identity_provider_plugins);

        let mut reg = self.registry.write().await;

        for source in sources {
            kinds.push(PluginKindEntry {
                category: PluginCategory::Source,
                kind: SourcePluginDescriptor::kind(&source).to_string(),
                config_version: SourcePluginDescriptor::config_version(&source).to_string(),
                config_schema_name: SourcePluginDescriptor::config_schema_name(&source).to_string(),
            });
            reg.register_source_with_metadata(Arc::new(source), plugin_id);
        }

        for reaction in reactions {
            kinds.push(PluginKindEntry {
                category: PluginCategory::Reaction,
                kind: ReactionPluginDescriptor::kind(&reaction).to_string(),
                config_version: ReactionPluginDescriptor::config_version(&reaction).to_string(),
                config_schema_name: ReactionPluginDescriptor::config_schema_name(&reaction)
                    .to_string(),
            });
            reg.register_reaction_with_metadata(Arc::new(reaction), plugin_id);
        }

        for bootstrap in bootstraps {
            kinds.push(PluginKindEntry {
                category: PluginCategory::Bootstrap,
                kind: BootstrapPluginDescriptor::kind(&bootstrap).to_string(),
                config_version: BootstrapPluginDescriptor::config_version(&bootstrap).to_string(),
                config_schema_name: BootstrapPluginDescriptor::config_schema_name(&bootstrap)
                    .to_string(),
            });
            reg.register_bootstrapper_with_metadata(Arc::new(bootstrap), plugin_id);
        }

        for identity_provider in identity_providers {
            kinds.push(PluginKindEntry {
                category: PluginCategory::IdentityProvider,
                kind: IdentityProviderPluginDescriptor::kind(&identity_provider).to_string(),
                config_version: IdentityProviderPluginDescriptor::config_version(
                    &identity_provider,
                )
                .to_string(),
                config_schema_name: IdentityProviderPluginDescriptor::config_schema_name(
                    &identity_provider,
                )
                .to_string(),
            });
            reg.register_identity_provider_with_metadata(Arc::new(identity_provider), plugin_id);
        }

        drop(reg);

        // Track state
        let state = LoadedPluginState {
            plugin_id: plugin_id.to_string(),
            status: PluginStatus::Loaded,
            kinds: kinds.clone(),
            metadata_info,
        };

        self.loaded_plugins
            .write()
            .await
            .insert(plugin_id.to_string(), state);

        // Emit event
        let _ = self.event_tx.send(PluginEvent::Loaded {
            plugin_id: plugin_id.to_string(),
            version: String::new(),
            kinds: kinds.clone(),
        });

        kinds
    }

    /// Load a single plugin from a file path and register its descriptors.
    ///
    /// This performs `dlopen`, metadata validation, `drasi_plugin_init()`, and
    /// descriptor registration. The plugin's library handle is intentionally
    /// leaked (never `dlclose`d) following the existing safety model.
    ///
    /// Returns the plugin ID and the kinds it provides.
    pub async fn load_plugin(
        &self,
        path: &Path,
        callback_context: Option<Arc<CallbackContext>>,
    ) -> anyhow::Result<(String, Vec<PluginKindEntry>)> {
        // Derive plugin_id from filename
        let plugin_id = path
            .file_name()
            .and_then(|f| f.to_str())
            .and_then(plugin_kind_from_filename)
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string()
            });

        // Load the plugin using host-sdk loader
        let (log_cb, log_ctx, lifecycle_cb, lifecycle_ctx) = match &callback_context {
            Some(ctx) => {
                let raw = ctx.clone().into_raw();
                (
                    callbacks::default_log_callback_fn(),
                    raw,
                    callbacks::default_lifecycle_callback_fn(),
                    raw,
                )
            }
            None => (
                callbacks::default_log_callback_fn(),
                std::ptr::null_mut(),
                callbacks::default_lifecycle_callback_fn(),
                std::ptr::null_mut(),
            ),
        };

        let loaded = load_plugin_from_path(path, log_ctx, log_cb, lifecycle_ctx, lifecycle_cb)?;

        let kinds = self.register_loaded_plugin(&plugin_id, loaded).await;

        Ok((plugin_id, kinds))
    }

    /// Update a plugin's status (for use by the orchestrator layer).
    pub async fn set_plugin_status(&self, plugin_id: &str, status: PluginStatus) {
        let mut plugins = self.loaded_plugins.write().await;
        if let Some(state) = plugins.get_mut(plugin_id) {
            state.status = status;
        }
    }

    /// Get the status of a loaded plugin.
    pub async fn get_plugin_status(&self, plugin_id: &str) -> Option<PluginStatus> {
        self.loaded_plugins
            .read()
            .await
            .get(plugin_id)
            .map(|s| s.status)
    }

    /// List all loaded plugins and their states.
    pub async fn list_plugins(&self) -> Vec<(String, PluginStatus, Vec<PluginKindEntry>)> {
        self.loaded_plugins
            .read()
            .await
            .values()
            .map(|s| (s.plugin_id.clone(), s.status, s.kinds.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_lifecycle_manager_creation() {
        let registry = Arc::new(RwLock::new(PluginRegistry::new()));
        let manager = PluginLifecycleManager::new(registry.clone());

        assert!(manager.list_plugins().await.is_empty());
    }

    #[tokio::test]
    async fn test_lifecycle_manager_subscribe() {
        let registry = Arc::new(RwLock::new(PluginRegistry::new()));
        let manager = PluginLifecycleManager::new(registry);

        let _rx = manager.subscribe();
    }

    #[tokio::test]
    async fn test_set_plugin_status() {
        let registry = Arc::new(RwLock::new(PluginRegistry::new()));
        let manager = PluginLifecycleManager::new(registry);

        // Insert a fake plugin state directly for testing
        {
            let mut plugins = manager.loaded_plugins.write().await;
            plugins.insert(
                "test-plugin".to_string(),
                LoadedPluginState {
                    plugin_id: "test-plugin".to_string(),
                    status: PluginStatus::Loaded,
                    kinds: vec![],
                    metadata_info: None,
                },
            );
        }

        assert_eq!(
            manager.get_plugin_status("test-plugin").await,
            Some(PluginStatus::Loaded)
        );

        manager
            .set_plugin_status("test-plugin", PluginStatus::Active)
            .await;

        assert_eq!(
            manager.get_plugin_status("test-plugin").await,
            Some(PluginStatus::Active)
        );
    }

    #[tokio::test]
    async fn test_get_plugin_status_nonexistent() {
        let registry = Arc::new(RwLock::new(PluginRegistry::new()));
        let manager = PluginLifecycleManager::new(registry);

        assert_eq!(manager.get_plugin_status("nonexistent").await, None);
    }

    #[tokio::test]
    async fn test_set_plugin_status_nonexistent_is_noop() {
        let registry = Arc::new(RwLock::new(PluginRegistry::new()));
        let manager = PluginLifecycleManager::new(registry);

        // Setting status on a nonexistent plugin should not panic
        manager
            .set_plugin_status("nonexistent", PluginStatus::Active)
            .await;

        // Still no plugins
        assert!(manager.list_plugins().await.is_empty());
    }
}
