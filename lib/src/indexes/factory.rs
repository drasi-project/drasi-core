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
use crate::plugin_core::IndexBackendPlugin;
use drasi_core::in_memory_index::in_memory_element_index::InMemoryElementIndex;
use drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue;
use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use drasi_core::interface::{ElementArchiveIndex, ElementIndex, FutureQueue, ResultIndex};
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

/// Set of indexes for a query
pub struct IndexSet {
    /// Element index for storing graph elements
    pub element_index: Arc<dyn ElementIndex>,
    /// Archive index for storing historical elements (for past() function)
    pub archive_index: Arc<dyn ElementArchiveIndex>,
    /// Result index for storing query results
    pub result_index: Arc<dyn ResultIndex>,
    /// Future queue for temporal queries
    pub future_queue: Arc<dyn FutureQueue>,
}

impl fmt::Debug for IndexSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexSet")
            .field("element_index", &"<trait object>")
            .field("archive_index", &"<trait object>")
            .field("result_index", &"<trait object>")
            .field("future_queue", &"<trait object>")
            .finish()
    }
}

/// Factory for creating index sets based on storage backend configuration
pub struct IndexFactory {
    /// Map of backend ID to backend specification
    backends: HashMap<String, StorageBackendSpec>,
    /// Optional index backend plugin for persistent storage (RocksDB, Redis/Garnet)
    plugin: Option<Arc<dyn IndexBackendPlugin>>,
}

impl fmt::Debug for IndexFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexFactory")
            .field("backends", &self.backends)
            .field("plugin", &self.plugin.as_ref().map(|_| "<plugin>"))
            .finish()
    }
}

impl IndexFactory {
    /// Create a new IndexFactory from a list of backend configurations
    ///
    /// # Arguments
    ///
    /// * `backends` - List of storage backend configurations
    /// * `plugin` - Optional index backend plugin for persistent storage (RocksDB, Redis/Garnet).
    ///   When using RocksDB or Redis backends, this plugin MUST be provided.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::indexes::{IndexFactory, StorageBackendConfig, StorageBackendSpec};
    /// // For in-memory only (no plugin needed)
    /// let backends = vec![
    ///     StorageBackendConfig {
    ///         id: "memory_test".to_string(),
    ///         spec: StorageBackendSpec::Memory {
    ///             enable_archive: true,
    ///         },
    ///     },
    /// ];
    /// let factory = IndexFactory::new(backends, None);
    ///
    /// // For persistent storage (plugin required)
    /// // use drasi_index_rocksdb::RocksDbIndexProvider;
    /// // let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
    /// // let factory = IndexFactory::new(backends, Some(Arc::new(provider)));
    /// ```
    pub fn new(
        backends: Vec<StorageBackendConfig>,
        plugin: Option<Arc<dyn IndexBackendPlugin>>,
    ) -> Self {
        let backends = backends.into_iter().map(|b| (b.id, b.spec)).collect();
        Self { backends, plugin }
    }

    /// Build an IndexSet for a query using the specified storage backend
    ///
    /// # Arguments
    /// * `backend_ref` - Reference to storage backend (named or inline)
    /// * `query_id` - Unique identifier for the query
    ///
    /// # Errors
    /// Returns `IndexError` if:
    /// - Named backend reference doesn't exist
    /// - Backend initialization fails (connection, path, etc.)
    /// - Invalid configuration
    pub async fn build(
        &self,
        backend_ref: &StorageBackendRef,
        query_id: &str,
    ) -> Result<IndexSet, IndexError> {
        let spec = match backend_ref {
            StorageBackendRef::Named(name) => self
                .backends
                .get(name)
                .ok_or_else(|| IndexError::UnknownStore(name.clone()))?,
            StorageBackendRef::Inline(spec) => spec,
        };

        self.build_from_spec(spec, query_id).await
    }

    /// Build an IndexSet from a storage backend specification
    async fn build_from_spec(
        &self,
        spec: &StorageBackendSpec,
        query_id: &str,
    ) -> Result<IndexSet, IndexError> {
        // Validate configuration before building
        spec.validate().map_err(IndexError::InitializationFailed)?;

        match spec {
            StorageBackendSpec::Memory { enable_archive } => {
                self.build_memory_indexes(*enable_archive)
            }
            StorageBackendSpec::RocksDb { .. } | StorageBackendSpec::Redis { .. } => {
                // Delegate to the plugin for persistent storage backends
                match &self.plugin {
                    Some(plugin) => self.build_from_plugin(plugin, query_id).await,
                    None => Err(IndexError::InitializationFailed(
                        "RocksDB or Redis backend requested but no index provider configured. \
                         Use DrasiLib::builder().with_index_provider(...) to provide one."
                            .to_string(),
                    )),
                }
            }
        }
    }

    /// Build in-memory indexes
    fn build_memory_indexes(&self, enable_archive: bool) -> Result<IndexSet, IndexError> {
        let mut element_index = InMemoryElementIndex::new();
        if enable_archive {
            element_index.enable_archive();
        }
        let element_index = Arc::new(element_index);
        let result_index = InMemoryResultIndex::new();
        let future_queue = InMemoryFutureQueue::new();

        Ok(IndexSet {
            element_index: element_index.clone(),
            archive_index: element_index,
            result_index: Arc::new(result_index),
            future_queue: Arc::new(future_queue),
        })
    }

    /// Build indexes using the provided plugin
    async fn build_from_plugin(
        &self,
        plugin: &Arc<dyn IndexBackendPlugin>,
        query_id: &str,
    ) -> Result<IndexSet, IndexError> {
        let element_index = plugin.create_element_index(query_id).await.map_err(|e| {
            log::error!("Failed to create element index for query '{query_id}': {e}");
            IndexError::InitializationFailed(format!(
                "Failed to create element index for query '{query_id}': {e}"
            ))
        })?;

        let archive_index = plugin.create_archive_index(query_id).await.map_err(|e| {
            log::error!("Failed to create archive index for query '{query_id}': {e}");
            IndexError::InitializationFailed(format!(
                "Failed to create archive index for query '{query_id}': {e}"
            ))
        })?;

        let result_index = plugin.create_result_index(query_id).await.map_err(|e| {
            log::error!("Failed to create result index for query '{query_id}': {e}");
            IndexError::InitializationFailed(format!(
                "Failed to create result index for query '{query_id}': {e}"
            ))
        })?;

        let future_queue = plugin.create_future_queue(query_id).await.map_err(|e| {
            log::error!("Failed to create future queue for query '{query_id}': {e}");
            IndexError::InitializationFailed(format!(
                "Failed to create future queue for query '{query_id}': {e}"
            ))
        })?;

        Ok(IndexSet {
            element_index,
            archive_index,
            result_index,
            future_queue,
        })
    }

    /// Check if a storage backend is volatile (requires re-bootstrap after restart)
    ///
    /// # Returns
    /// - `true` for Memory backend (no persistence)
    /// - `false` for RocksDB and Redis backends (persistent)
    pub fn is_volatile(&self, backend_ref: &StorageBackendRef) -> bool {
        let spec = match backend_ref {
            StorageBackendRef::Named(name) => match self.backends.get(name) {
                Some(spec) => spec,
                None => return false, // Unknown backend, assume not volatile
            },
            StorageBackendRef::Inline(spec) => spec,
        };

        spec.is_volatile()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_factory_new() {
        let backends = vec![
            StorageBackendConfig {
                id: "memory_test".to_string(),
                spec: StorageBackendSpec::Memory {
                    enable_archive: true,
                },
            },
            StorageBackendConfig {
                id: "rocks_test".to_string(),
                spec: StorageBackendSpec::RocksDb {
                    path: "/tmp/test".to_string(),
                    enable_archive: false,
                    direct_io: false,
                },
            },
        ];

        let factory = IndexFactory::new(backends, None);
        assert_eq!(factory.backends.len(), 2);
        assert!(factory.backends.contains_key("memory_test"));
        assert!(factory.backends.contains_key("rocks_test"));
    }

    #[tokio::test]
    async fn test_build_memory_indexes() {
        let backends = vec![StorageBackendConfig {
            id: "memory_test".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: true,
            },
        }];

        let factory = IndexFactory::new(backends, None);
        let backend_ref = StorageBackendRef::Named("memory_test".to_string());
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_unknown_backend() {
        let factory = IndexFactory::new(vec![], None);
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
    async fn test_build_inline_memory() {
        let factory = IndexFactory::new(vec![], None);
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: false,
        });
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_rocksdb_without_plugin_errors() {
        // Verify that attempting to use RocksDB without a plugin returns an error
        let factory = IndexFactory::new(vec![], None);
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
            path: "/data/test".to_string(),
            enable_archive: false,
            direct_io: false,
        });
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::InitializationFailed(msg) => {
                assert!(msg.contains("no index provider configured"));
            }
            _ => panic!("Expected InitializationFailed error"),
        }
    }

    #[tokio::test]
    async fn test_build_redis_without_plugin_errors() {
        // Verify that attempting to use Redis without a plugin returns an error
        let factory = IndexFactory::new(vec![], None);
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Redis {
            connection_string: "redis://localhost:6379".to_string(),    // DevSkim: ignore DS162092
            cache_size: None,
        });
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            IndexError::InitializationFailed(msg) => {
                assert!(msg.contains("no index provider configured"));
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

        let factory = IndexFactory::new(backends, None);
        let backend_ref = StorageBackendRef::Named("memory_test".to_string());
        assert!(factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_rocksdb() {
        let backends = vec![StorageBackendConfig {
            id: "rocks_test".to_string(),
            spec: StorageBackendSpec::RocksDb {
                path: "/data/test".to_string(),
                enable_archive: false,
                direct_io: false,
            },
        }];

        let factory = IndexFactory::new(backends, None);
        let backend_ref = StorageBackendRef::Named("rocks_test".to_string());
        assert!(!factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_inline() {
        let factory = IndexFactory::new(vec![], None);
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: false,
        });
        assert!(factory.is_volatile(&backend_ref));

        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
            path: "/data/test".to_string(),
            enable_archive: false,
            direct_io: false,
        });
        assert!(!factory.is_volatile(&backend_ref));
    }
}
