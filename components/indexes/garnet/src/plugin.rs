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

//! Garnet/Redis Index Backend Plugin
//!
//! This module provides the `GarnetIndexProvider` which implements the
//! `IndexBackendPlugin` trait for Redis/Garnet-based distributed storage.
//!
//! # Example
//!
//! ```ignore
//! use drasi_index_garnet::GarnetIndexProvider;
//! use drasi_lib::DrasiLib;
//! use std::sync::Arc;
//!
//! let provider = GarnetIndexProvider::new("redis://localhost:6379", None);
//! let drasi = DrasiLib::builder()
//!     .with_index_provider(Arc::new(provider))
//!     .build()?;
//! ```

use async_trait::async_trait;
use drasi_core::interface::{
    ElementArchiveIndex, ElementIndex, FutureQueue, IndexBackendPlugin, IndexError, ResultIndex,
};
use std::sync::Arc;

use crate::element_index::GarnetElementIndex;
use crate::future_queue::GarnetFutureQueue;
use crate::result_index::GarnetResultIndex;

/// Garnet/Redis index backend provider.
///
/// This provider creates Redis/Garnet-backed indexes for distributed storage.
/// Data survives restarts and can be shared across multiple instances.
///
/// # Configuration
///
/// - `connection_string`: Redis connection URL (e.g., "redis://localhost:6379")
/// - `cache_size`: Optional local cache size for improved read performance
///
/// # Key Structure
///
/// The provider uses the following Redis key patterns:
/// ```text
/// ei:{query_id}:{source_id}:{element_id}  - Element data
/// ari:{query_id}:{set_id}                 - Accumulator/result data
/// fqi:{query_id}                          - Future queue
/// ```
///
/// # Caching
///
/// When `cache_size` is specified, a local LRU cache is added in front of Redis
/// for frequently accessed elements and results. This improves read performance
/// but adds memory overhead.
pub struct GarnetIndexProvider {
    connection_string: String,
    cache_size: Option<usize>,
}

impl GarnetIndexProvider {
    /// Create a new Garnet/Redis index provider.
    ///
    /// # Arguments
    ///
    /// * `connection_string` - Redis connection URL (e.g., "redis://localhost:6379")
    /// * `cache_size` - Optional local cache size for improved read performance
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Without caching
    /// let provider = GarnetIndexProvider::new("redis://localhost:6379", None);
    ///
    /// // With caching (1000 element cache)
    /// let provider = GarnetIndexProvider::new("redis://localhost:6379", Some(1000));
    /// ```
    pub fn new(connection_string: impl Into<String>, cache_size: Option<usize>) -> Self {
        Self {
            connection_string: connection_string.into(),
            cache_size,
        }
    }

    /// Get the configured connection string.
    pub fn connection_string(&self) -> &str {
        &self.connection_string
    }

    /// Get the configured cache size.
    pub fn cache_size(&self) -> Option<usize> {
        self.cache_size
    }
}

#[async_trait]
impl IndexBackendPlugin for GarnetIndexProvider {
    async fn create_element_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ElementIndex>, IndexError> {
        let index = GarnetElementIndex::connect(query_id, &self.connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet element index for query '{}' at '{}': {}",
                    query_id,
                    self.connection_string,
                    e
                );
                e
            })?;

        // Note: Caching is handled at the factory level in lib, not here
        // This allows the factory to wrap with CachedElementIndex if cache_size is set
        Ok(Arc::new(index))
    }

    async fn create_archive_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ElementArchiveIndex>, IndexError> {
        // Garnet shares the element index and archive index
        // Archive functionality is managed within GarnetElementIndex
        let index = GarnetElementIndex::connect(query_id, &self.connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet archive index for query '{}' at '{}': {}",
                    query_id,
                    self.connection_string,
                    e
                );
                e
            })?;

        Ok(Arc::new(index))
    }

    async fn create_result_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ResultIndex>, IndexError> {
        let index = GarnetResultIndex::connect(query_id, &self.connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet result index for query '{}' at '{}': {}",
                    query_id,
                    self.connection_string,
                    e
                );
                e
            })?;

        // Note: Caching is handled at the factory level if cache_size is set
        Ok(Arc::new(index))
    }

    async fn create_future_queue(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn FutureQueue>, IndexError> {
        let queue = GarnetFutureQueue::connect(query_id, &self.connection_string)
            .await
            .map_err(|e| {
                log::error!(
                    "Failed to connect to Redis/Garnet future queue for query '{}' at '{}': {}",
                    query_id,
                    self.connection_string,
                    e
                );
                e
            })?;

        Ok(Arc::new(queue))
    }

    fn is_volatile(&self) -> bool {
        false // Redis/Garnet is persistent (assuming persistence is configured)
    }
}
