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
use drasi_core::in_memory_index::in_memory_element_index::InMemoryElementIndex;
use drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue;
use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use drasi_core::index_cache::cached_element_index::CachedElementIndex;
use drasi_core::index_cache::cached_result_index::CachedResultIndex;
use drasi_core::interface::{ElementArchiveIndex, ElementIndex, FutureQueue, ResultIndex};
use drasi_index_garnet::element_index::GarnetElementIndex;
use drasi_index_garnet::future_queue::GarnetFutureQueue;
use drasi_index_garnet::result_index::GarnetResultIndex;
use drasi_index_rocksdb::element_index::{RocksDbElementIndex, RocksIndexOptions};
use drasi_index_rocksdb::future_queue::RocksDbFutureQueue;
use drasi_index_rocksdb::result_index::RocksDbResultIndex;
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
                write!(f, "Unknown storage backend: '{}'. Check that the backend is defined in storage_backends configuration.", name)
            }
            IndexError::ConnectionFailed(details) => {
                write!(f, "Failed to connect to storage backend: {}", details)
            }
            IndexError::PathError(details) => {
                write!(f, "Storage path error: {}", details)
            }
            IndexError::InitializationFailed(details) => {
                write!(f, "Failed to initialize storage backend: {}", details)
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
#[derive(Debug)]
pub struct IndexFactory {
    /// Map of backend ID to backend specification
    backends: HashMap<String, StorageBackendSpec>,
}

impl IndexFactory {
    /// Create a new IndexFactory from a list of backend configurations
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::indexes::{IndexFactory, StorageBackendConfig, StorageBackendSpec};
    /// let backends = vec![
    ///     StorageBackendConfig {
    ///         id: "rocks_persistent".to_string(),
    ///         spec: StorageBackendSpec::RocksDb {
    ///             path: "/data/drasi".to_string(),
    ///             enable_archive: true,
    ///             direct_io: false,
    ///         },
    ///     },
    /// ];
    /// let factory = IndexFactory::new(backends);
    /// ```
    pub fn new(backends: Vec<StorageBackendConfig>) -> Self {
        let backends = backends.into_iter().map(|b| (b.id, b.spec)).collect();
        Self { backends }
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
        spec.validate()
            .map_err(|e| IndexError::InitializationFailed(e))?;

        match spec {
            StorageBackendSpec::Memory { enable_archive } => {
                self.build_memory_indexes(*enable_archive)
            }
            StorageBackendSpec::RocksDb {
                path,
                enable_archive,
                direct_io,
            } => self.build_rocksdb_indexes(query_id, path, *enable_archive, *direct_io),
            StorageBackendSpec::Redis {
                connection_string,
                cache_size,
            } => {
                self.build_redis_indexes(query_id, connection_string, *cache_size)
                    .await
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

    /// Build RocksDB indexes
    fn build_rocksdb_indexes(
        &self,
        query_id: &str,
        path: &str,
        enable_archive: bool,
        direct_io: bool,
    ) -> Result<IndexSet, IndexError> {
        let options = RocksIndexOptions {
            archive_enabled: enable_archive,
            direct_io,
        };

        let element_index = RocksDbElementIndex::new(query_id, path, options).map_err(|e| {
            log::error!(
                "Failed to create RocksDB element index for query '{}' at path '{}': {}",
                query_id,
                path,
                e
            );
            IndexError::PathError(format!(
                "Failed to initialize RocksDB at '{}' for query '{}': {}",
                path, query_id, e
            ))
        })?;
        let element_index = Arc::new(element_index);

        let result_index = RocksDbResultIndex::new(query_id, path).map_err(|e| {
            log::error!(
                "Failed to create RocksDB result index for query '{}' at path '{}': {}",
                query_id,
                path,
                e
            );
            IndexError::PathError(format!(
                "Failed to initialize RocksDB result index at '{}' for query '{}': {}",
                path, query_id, e
            ))
        })?;
        let result_index = Arc::new(result_index);

        let future_queue = RocksDbFutureQueue::new(query_id, path).map_err(|e| {
            log::error!(
                "Failed to create RocksDB future queue for query '{}' at path '{}': {}",
                query_id,
                path,
                e
            );
            IndexError::PathError(format!(
                "Failed to initialize RocksDB future queue at '{}' for query '{}': {}",
                path, query_id, e
            ))
        })?;
        let future_queue = Arc::new(future_queue);

        Ok(IndexSet {
            element_index: element_index.clone(),
            archive_index: element_index,
            result_index,
            future_queue,
        })
    }

    /// Build Redis/Garnet indexes with optional caching
    async fn build_redis_indexes(
        &self,
        query_id: &str,
        connection_string: &str,
        cache_size: Option<usize>,
    ) -> Result<IndexSet, IndexError> {
        let element_index = GarnetElementIndex::connect(query_id, connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet element index for query '{}' at '{}': {}",
                    query_id,
                    connection_string,
                    e
                );
                IndexError::ConnectionFailed(format!(
                    "Failed to connect to Redis/Garnet at '{}' for query '{}': {}",
                    connection_string, query_id, e
                ))
            })?;
        let element_index = Arc::new(element_index);

        let result_index = GarnetResultIndex::connect(query_id, connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet result index for query '{}' at '{}': {}",
                    query_id,
                    connection_string,
                    e
                );
                IndexError::ConnectionFailed(format!(
                    "Failed to connect to Redis/Garnet result index at '{}' for query '{}': {}",
                    connection_string, query_id, e
                ))
            })?;
        let result_index = Arc::new(result_index);

        let future_queue = GarnetFutureQueue::connect(query_id, connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet future queue for query '{}' at '{}': {}",
                    query_id,
                    connection_string,
                    e
                );
                IndexError::ConnectionFailed(format!(
                    "Failed to connect to Redis/Garnet future queue at '{}' for query '{}': {}",
                    connection_string, query_id, e
                ))
            })?;
        let future_queue = Arc::new(future_queue);

        // Apply caching if configured
        match cache_size {
            Some(cache_size) => {
                let cached_element_index =
                    CachedElementIndex::new(element_index.clone(), cache_size).map_err(|e| {
                        log::error!(
                            "Failed to create cached element index for query '{}': {}",
                            query_id,
                            e
                        );
                        IndexError::NotSupported
                    })?;

                let cached_result_index = CachedResultIndex::new(result_index, cache_size)
                    .map_err(|e| {
                        log::error!(
                            "Failed to create cached result index for query '{}': {}",
                            query_id,
                            e
                        );
                        IndexError::NotSupported
                    })?;

                Ok(IndexSet {
                    element_index: Arc::new(cached_element_index),
                    archive_index: element_index,
                    result_index: Arc::new(cached_result_index),
                    future_queue,
                })
            }
            None => Ok(IndexSet {
                element_index: element_index.clone(),
                archive_index: element_index,
                result_index,
                future_queue,
            }),
        }
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

        let factory = IndexFactory::new(backends);
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

        let factory = IndexFactory::new(backends);
        let backend_ref = StorageBackendRef::Named("memory_test".to_string());
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_build_unknown_backend() {
        let factory = IndexFactory::new(vec![]);
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
        let factory = IndexFactory::new(vec![]);
        let backend_ref = StorageBackendRef::Inline(StorageBackendSpec::Memory {
            enable_archive: false,
        });
        let result = factory.build(&backend_ref, "test_query").await;

        assert!(result.is_ok());
    }

    #[test]
    fn test_is_volatile_memory() {
        let backends = vec![StorageBackendConfig {
            id: "memory_test".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        }];

        let factory = IndexFactory::new(backends);
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

        let factory = IndexFactory::new(backends);
        let backend_ref = StorageBackendRef::Named("rocks_test".to_string());
        assert!(!factory.is_volatile(&backend_ref));
    }

    #[test]
    fn test_is_volatile_inline() {
        let factory = IndexFactory::new(vec![]);
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
