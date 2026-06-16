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

use crate::indexes::config::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};
use crate::indexes::IndexBackendPlugin;
use drasi_core::in_memory_index::in_memory_element_index::InMemoryElementIndex;
use drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue;
use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use drasi_core::interface::{CreatedIndexes, IndexSet, NoOpSessionControl};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

/// Error type for index factory operations
#[derive(Debug)]
pub enum IndexError {
    /// Referenced storage backend does not exist
    UnknownStore(String),
    /// Failed to connect to Redis/Garnet
    ConnectionFailed(String),
    /// RocksDB path error (doesn't exist, no permissions, etc.)
    PathError(String),
    /// Generic initialization failure
    InitializationFailed(String),
    /// Feature not supported
    NotSupported,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::UnknownStore(name) => {
                write!(f, "Unknown storage backend: '{name}'. Check that the backend is defined in storage_backends configuration.")
            }
            IndexError::ConnectionFailed(details) => {
                write!(f, "Failed to connect to storage backend: {details}")
            }
            IndexError::PathError(details) => {
                write!(f, "Storage path error: {details}")
            }
            IndexError::InitializationFailed(details) => {
                write!(f, "Failed to initialize storage backend: {details}")
            }
            IndexError::NotSupported => {
                write!(f, "Operation not supported")
            }
        }
    }
}

impl std::error::Error for IndexError {}

impl From<drasi_core::interface::IndexError> for IndexError {
    fn from(err: drasi_core::interface::IndexError) -> Self {
        IndexError::InitializationFailed(err.to_string())
    }
}

/// Factory for creating index sets based on storage backend configuration.
///
/// In-memory backends are created natively. Persistent ("plugin") backends are
/// served by named [`IndexBackendPlugin`] providers injected at build time via
/// `DrasiLibBuilder::with_index_provider(name, provider)`. A query references a
/// backend by name; the factory resolves it to an injected provider, a declared
/// in-memory backend, or (if neither) reports a clear error.
pub struct IndexFactory {
    /// Declared in-memory backends: id -> enable_archive
    memory_backends: HashMap<String, bool>,
    /// Declared plugin backends: id -> kind (used only for diagnostics)
    plugin_backends: HashMap<String, String>,
    /// Injected named providers: name -> provider
    providers: HashMap<String, Arc<dyn IndexBackendPlugin>>,
    /// Default backend applied to queries whose `storage_backend` is `None`.
    /// When set (typically to a [`StorageBackendRef::Named`] pointing at an
    /// injected provider), a query that does not specify a backend uses this
    /// instead of falling back to native in-memory indexes.
    default_backend: Option<StorageBackendRef>,
}

impl fmt::Debug for IndexFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexFactory")
            .field("memory_backends", &self.memory_backends)
            .field("plugin_backends", &self.plugin_backends)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("default_backend", &self.default_backend)
            .finish()
    }
}

impl IndexFactory {
    /// Create a new IndexFactory from declared backends and injected providers.
    ///
    /// # Arguments
    ///
    /// * `backends` - List of storage backend configurations. `Memory` backends are
    ///   created natively; `Plugin` backends are recorded for diagnostics and must be
    ///   satisfied by an injected provider of the same name.
    /// * `providers` - Map of provider name -> [`IndexBackendPlugin`]. A query that
    ///   references a name present here is served by that provider.
    ///
    /// # Example
    /// ```no_run
    /// # use std::collections::HashMap;
    /// # use drasi_lib::indexes::{IndexFactory, StorageBackendConfig, StorageBackendSpec};
    /// // For in-memory only (no providers needed)
    /// let backends = vec![
    ///     StorageBackendConfig {
    ///         id: "memory_test".to_string(),
    ///         spec: StorageBackendSpec::Memory {
    ///             enable_archive: true,
    ///         },
    ///     },
    /// ];
    /// let factory = IndexFactory::new(backends, HashMap::new());
    ///
    /// // For persistent storage, inject a named provider:
    /// // use drasi_index_rocksdb::RocksDbIndexProvider;
    /// // let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
    /// // let providers = HashMap::from([("rocks".to_string(), Arc::new(provider) as _)]);
    /// // let factory = IndexFactory::new(vec![], providers);
    /// ```
    pub fn new(
        backends: Vec<StorageBackendConfig>,
        providers: HashMap<String, Arc<dyn IndexBackendPlugin>>,
    ) -> Self {
        Self::new_with_default(backends, providers, None)
    }

    /// Create a new IndexFactory with an optional default backend.
    ///
    /// The `default_backend` is applied to queries whose `storage_backend` is
    /// `None`. This is how `DrasiLibBuilder::with_default_index_provider` makes
    /// a single injected provider cover every query that does not name a backend
    /// explicitly. When `default_backend` is `None`, unspecified queries fall
    /// back to native in-memory indexes (the standard behavior).
    pub fn new_with_default(
        backends: Vec<StorageBackendConfig>,
        providers: HashMap<String, Arc<dyn IndexBackendPlugin>>,
        default_backend: Option<StorageBackendRef>,
    ) -> Self {
        let mut memory_backends = HashMap::new();
        let mut plugin_backends = HashMap::new();
        for b in backends {
            match b.spec {
                StorageBackendSpec::Memory { enable_archive } => {
                    memory_backends.insert(b.id, enable_archive);
                }
                StorageBackendSpec::Plugin { kind, .. } => {
                    plugin_backends.insert(b.id, kind);
                }
            }
        }
        Self {
            memory_backends,
            plugin_backends,
            providers,
            default_backend,
        }
    }

    /// The default backend applied to queries with no `storage_backend`, if any.
    pub fn default_backend(&self) -> Option<&StorageBackendRef> {
        self.default_backend.as_ref()
    }

    /// Build a CreatedIndexes for a query using the specified storage backend
    ///
    /// # Arguments
    /// * `backend_ref` - Reference to storage backend (named or inline)
    /// * `query_id` - Unique identifier for the query
    ///
    /// # Errors
    /// Returns `IndexError` if:
    /// - Named backend reference doesn't resolve to a provider or in-memory backend
    /// - A declared plugin backend has no injected provider
    /// - Backend initialization fails
    pub async fn build(
        &self,
        backend_ref: &StorageBackendRef,
        query_id: &str,
    ) -> Result<CreatedIndexes, IndexError> {
        match backend_ref {
            StorageBackendRef::Named(name) => {
                if let Some(provider) = self.providers.get(name) {
                    self.build_from_plugin(provider, query_id).await
                } else if let Some(enable_archive) = self.memory_backends.get(name) {
                    self.build_memory_indexes(*enable_archive)
                } else if let Some(kind) = self.plugin_backends.get(name) {
                    Err(IndexError::InitializationFailed(format!(
                        "Storage backend '{name}' (kind '{kind}') is declared but no index provider \
                         was injected for it. Use DrasiLib::builder().with_index_provider(\"{name}\", ...) \
                         to provide one."
                    )))
                } else {
                    Err(IndexError::UnknownStore(name.clone()))
                }
            }
            StorageBackendRef::Inline(StorageBackendSpec::Memory { enable_archive }) => {
                self.build_memory_indexes(*enable_archive)
            }
            StorageBackendRef::Inline(StorageBackendSpec::Plugin { kind, .. }) => {
                Err(IndexError::InitializationFailed(format!(
                    "Inline plugin storage backend (kind '{kind}') is not supported in embedded mode. \
                     Declare a named storage backend and inject a provider via \
                     DrasiLib::builder().with_index_provider(name, ...)."
                )))
            }
        }
    }

    /// Build in-memory indexes (returns checkpoint_store: None — caller provides InMemoryCheckpointStore)
    fn build_memory_indexes(&self, enable_archive: bool) -> Result<CreatedIndexes, IndexError> {
        let mut element_index = InMemoryElementIndex::new();
        if enable_archive {
            element_index.enable_archive();
        }
        let element_index = Arc::new(element_index);
        let result_index = InMemoryResultIndex::new();
        let future_queue = InMemoryFutureQueue::new();

        Ok(CreatedIndexes {
            set: IndexSet {
                element_index: element_index.clone(),
                archive_index: element_index,
                result_index: Arc::new(result_index),
                future_queue: Arc::new(future_queue),
                session_control: Arc::new(NoOpSessionControl),
            },
            checkpoint_store: None,
            outbox_writer: None,
            live_results_writer: None,
        })
    }

    /// Build indexes using the provided plugin
    async fn build_from_plugin(
        &self,
        plugin: &Arc<dyn IndexBackendPlugin>,
        query_id: &str,
    ) -> Result<CreatedIndexes, IndexError> {
        plugin.create_indexes(query_id).await.map_err(|e| {
            log::error!("Failed to create indexes for query '{query_id}': {e}");
            IndexError::InitializationFailed(format!(
                "Failed to create indexes for query '{query_id}': {e}"
            ))
        })
    }

    /// Check if a storage backend is volatile (requires re-bootstrap after restart)
    ///
    /// # Returns
    /// - `true` for in-memory backends (no persistence)
    /// - The provider's own `is_volatile()` for injected plugin providers
    /// - `false` for declared-but-uninjected plugin backends and unknown names
    ///   (persistent assumption is safer than treating them as volatile)
    pub fn is_volatile(&self, backend_ref: &StorageBackendRef) -> bool {
        match backend_ref {
            StorageBackendRef::Named(name) => {
                if let Some(provider) = self.providers.get(name) {
                    provider.is_volatile()
                } else if self.memory_backends.contains_key(name) {
                    true
                } else {
                    // Declared-but-uninjected plugin backend or unknown name.
                    false
                }
            }
            StorageBackendRef::Inline(spec) => spec.is_volatile(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    /// Minimal in-memory-backed mock provider for tests.
    struct MockPlugin {
        volatile: bool,
    }

    #[async_trait]
    impl IndexBackendPlugin for MockPlugin {
        async fn create_indexes(
            &self,
            _query_id: &str,
        ) -> Result<drasi_core::interface::CreatedIndexes, drasi_core::interface::IndexError>
        {
            let element_index = Arc::new(InMemoryElementIndex::new());
            Ok(CreatedIndexes {
                set: IndexSet {
                    element_index: element_index.clone(),
                    archive_index: element_index,
                    result_index: Arc::new(InMemoryResultIndex::new()),
                    future_queue: Arc::new(InMemoryFutureQueue::new()),
                    session_control: Arc::new(NoOpSessionControl),
                },
                checkpoint_store: None,
                outbox_writer: None,
                live_results_writer: None,
            })
        }

        fn is_volatile(&self) -> bool {
            self.volatile
        }
    }

    fn providers_with(name: &str, volatile: bool) -> HashMap<String, Arc<dyn IndexBackendPlugin>> {
        let mut m: HashMap<String, Arc<dyn IndexBackendPlugin>> = HashMap::new();
        m.insert(name.to_string(), Arc::new(MockPlugin { volatile }));
        m
    }

    #[test]
    fn test_index_factory_default_backend_accessor() {
        // Without a default, the accessor returns None.
        let factory = IndexFactory::new(vec![], HashMap::new());
        assert!(factory.default_backend().is_none());

        // With a default, the accessor returns the configured ref.
        let factory = IndexFactory::new_with_default(
            vec![],
            providers_with("rocks", false),
            Some(StorageBackendRef::Named("rocks".to_string())),
        );
        match factory.default_backend() {
            Some(StorageBackendRef::Named(name)) => assert_eq!(name, "rocks"),
            other => panic!("expected default Named(\"rocks\"), got {other:?}"),
        }
    }

    #[test]
    fn test_index_factory_new_splits_backends() {
        let backends = vec![
            StorageBackendConfig {
                id: "memory_test".to_string(),
                spec: StorageBackendSpec::Memory {
                    enable_archive: true,
                },
            },
            StorageBackendConfig {
                id: "rocks_test".to_string(),
                spec: StorageBackendSpec::Plugin {
                    kind: "rocksdb".to_string(),
                    config: serde_json::json!({ "path": "/tmp/test" }),
                },
            },
        ];

        let factory = IndexFactory::new(backends, HashMap::new());
        assert_eq!(factory.memory_backends.len(), 1);
        assert!(factory.memory_backends.contains_key("memory_test"));
        assert_eq!(factory.plugin_backends.len(), 1);
        assert_eq!(
            factory
                .plugin_backends
                .get("rocks_test")
                .map(|s| s.as_str()),
            Some("rocksdb")
        );
    }

    #[tokio::test]
    async fn test_build_memory_indexes() {
        let backends = vec![StorageBackendConfig {
            id: "memory_test".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: true,
            },
        }];

        let factory = IndexFactory::new(backends, HashMap::new());
        let backend_ref = StorageBackendRef::Named("memory_test".to_string());
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_named_provider() {
        let factory = IndexFactory::new(vec![], providers_with("rocks", false));
        let backend_ref = StorageBackendRef::Named("rocks".to_string());
        let result = factory.build(&backend_ref, "test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_unknown_backend() {
        let factory = IndexFactory::new(vec![], HashMap::new());
        let backend_ref = StorageBackendRef::Named("nonexistent".to_string());
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::UnknownStore(name) => {
                assert_eq!(name, "nonexistent");
            }
            _ => panic!("Expected UnknownStore error"),
        }
    }

    #[tokio::test]
    async fn test_build_declared_plugin_without_provider_errors() {
        // A Plugin backend declared in config but with no injected provider.
        let backends = vec![StorageBackendConfig {
            id: "rocks".to_string(),
            spec: StorageBackendSpec::Plugin {
                kind: "rocksdb".to_string(),
                config: serde_json::json!({ "path": "/data/test" }),
            },
        }];
        let factory = IndexFactory::new(backends, HashMap::new());
        let backend_ref = StorageBackendRef::Named("rocks".to_string());
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::InitializationFailed(msg) => {
                assert!(msg.contains("no index provider"));
                assert!(msg.contains("rocks"));
            }
            _ => panic!("Expected InitializationFailed error"),
        }
    }

    #[tokio::test]
    async fn test_build_inline_memory() {
        let factory = IndexFactory::new(vec![], HashMap::new());
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: false,
        });
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_inline_plugin_errors() {
        let factory = IndexFactory::new(vec![], providers_with("rocks", false));
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Plugin {
            kind: "rocksdb".to_string(),
            config: serde_json::json!({ "path": "/data/test" }),
        });
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::InitializationFailed(msg) => {
                assert!(msg.contains("Inline plugin"));
            }
            _ => panic!("Expected InitializationFailed error"),
        }
    }

    #[test]
    fn test_is_volatile_memory() {
        let backends = vec![StorageBackendConfig {
            id: "memory_test".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        }];

        let factory = IndexFactory::new(backends, HashMap::new());
        let backend_ref = StorageBackendRef::Named("memory_test".to_string());
        assert!(factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_provider_delegates() {
        let factory = IndexFactory::new(vec![], providers_with("rocks", false));
        let backend_ref = StorageBackendRef::Named("rocks".to_string());
        assert!(!factory.is_volatile(&backend_ref));

        let factory = IndexFactory::new(vec![], providers_with("vol", true));
        let backend_ref = StorageBackendRef::Named("vol".to_string());
        assert!(factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_declared_plugin_without_provider() {
        let backends = vec![StorageBackendConfig {
            id: "rocks".to_string(),
            spec: StorageBackendSpec::Plugin {
                kind: "rocksdb".to_string(),
                config: serde_json::json!({ "path": "/data/test" }),
            },
        }];
        let factory = IndexFactory::new(backends, HashMap::new());
        let backend_ref = StorageBackendRef::Named("rocks".to_string());
        // Uninjected plugin backend -> persistent assumption (not volatile)
        assert!(!factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_inline() {
        let factory = IndexFactory::new(vec![], HashMap::new());
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: false,
        });
        assert!(factory.is_volatile(&backend_ref));

        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Plugin {
            kind: "rocksdb".to_string(),
            config: serde_json::json!({ "path": "/data/test" }),
        });
        assert!(!factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_unknown_backend() {
        // When a named backend doesn't exist, is_volatile returns false
        let factory = IndexFactory::new(vec![], HashMap::new());
        let backend_ref = StorageBackendRef::Named("nonexistent".to_string());
        assert!(!factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_index_error_display_unknown_store() {
        let error = IndexError::UnknownStore("my_backend".to_string());
        let display = format!("{error}");
        assert!(display.contains("Unknown storage backend"));
        assert!(display.contains("my_backend"));
    }

    #[test]
    fn test_index_error_display_connection_failed() {
        let error = IndexError::ConnectionFailed("Connection refused".to_string());
        let display = format!("{error}");
        assert!(display.contains("Failed to connect"));
        assert!(display.contains("Connection refused"));
    }

    #[test]
    fn test_index_error_display_path_error() {
        let error = IndexError::PathError("/invalid/path".to_string());
        let display = format!("{error}");
        assert!(display.contains("Storage path error"));
        assert!(display.contains("/invalid/path"));
    }

    #[test]
    fn test_index_error_display_initialization_failed() {
        let error = IndexError::InitializationFailed("Database init failed".to_string());
        let display = format!("{error}");
        assert!(display.contains("Failed to initialize"));
        assert!(display.contains("Database init failed"));
    }

    #[test]
    fn test_index_error_display_not_supported() {
        let error = IndexError::NotSupported;
        let display = format!("{error}");
        assert!(display.contains("not supported"));
    }

    #[test]
    fn test_index_error_is_std_error() {
        let error = IndexError::UnknownStore("test".to_string());
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &error;
    }

    #[test]
    fn test_index_error_from_drasi_core_index_error() {
        // Create a std::io::Error to wrap in IndexError::other
        let io_error = std::io::Error::new(std::io::ErrorKind::Other, "test error");
        let core_error = drasi_core::interface::IndexError::other(io_error);
        let error: IndexError = core_error.into();
        match error {
            IndexError::InitializationFailed(msg) => {
                assert!(msg.contains("test error"));
            }
            _ => panic!("Expected InitializationFailed error"),
        }
    }

    #[test]
    fn test_index_set_debug() {
        // We can't easily construct an IndexSet without going through the factory,
        // but we can test via build
        let backends = vec![StorageBackendConfig {
            id: "memory_test".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        }];
        let factory = IndexFactory::new(backends, HashMap::new());

        // Use tokio runtime for async test
        let rt = tokio::runtime::Runtime::new().unwrap();
        let index_set = rt
            .block_on(factory.build(&StorageBackendRef::Named("memory_test".to_string()), "q1"))
            .unwrap();

        let debug_str = format!("{index_set:?}");
        assert!(debug_str.contains("IndexSet"));
        assert!(debug_str.contains("element_index"));
        assert!(debug_str.contains("archive_index"));
        assert!(debug_str.contains("result_index"));
        assert!(debug_str.contains("future_queue"));
    }

    #[test]
    fn test_index_factory_debug() {
        let backends = vec![StorageBackendConfig {
            id: "memory_test".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: true,
            },
        }];
        let factory = IndexFactory::new(backends, HashMap::new());
        let debug_str = format!("{factory:?}");
        assert!(debug_str.contains("IndexFactory"));
        assert!(debug_str.contains("memory_backends"));
        assert!(debug_str.contains("memory_test"));
    }

    #[test]
    fn test_index_factory_debug_with_provider() {
        let factory = IndexFactory::new(vec![], providers_with("rocks", false));
        let debug_str = format!("{factory:?}");
        assert!(debug_str.contains("IndexFactory"));
        assert!(debug_str.contains("providers"));
        assert!(debug_str.contains("rocks"));
    }

    #[tokio::test]
    async fn test_build_memory_without_archive() {
        let factory = IndexFactory::new(vec![], HashMap::new());
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: false,
        });
        let result = factory.build(&backend_ref, "test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_memory_with_archive() {
        let factory = IndexFactory::new(vec![], HashMap::new());
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: true,
        });
        let result = factory.build(&backend_ref, "test_query").await;
        assert!(result.is_ok());
    }
}
