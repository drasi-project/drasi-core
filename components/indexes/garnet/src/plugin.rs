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
//! let provider = GarnetIndexProvider::new("redis://localhost:6379", None, true);
//! let drasi = DrasiLib::builder()
//!     .with_index_provider(Arc::new(provider))
//!     .build()?;
//! ```

use async_trait::async_trait;
use drasi_core::interface::{IndexBackendPlugin, IndexError, IndexSet};
use std::sync::Arc;

use crate::element_index::GarnetElementIndex;
use crate::future_queue::GarnetFutureQueue;
use crate::result_index::GarnetResultIndex;
use crate::session_state::{GarnetSessionControl, GarnetSessionState};

/// Garnet/Redis index backend provider.
///
/// This provider creates Redis/Garnet-backed indexes for distributed storage.
/// All indexes for a query share a single `MultiplexedConnection`, reducing
/// connection overhead.
///
/// # Configuration
///
/// - `connection_string`: Redis connection URL (e.g., "redis://localhost:6379")
/// - `cache_size`: Optional local cache size for improved read performance
/// - `enable_archive`: Enable archive index for `past()` function support
///
/// # Key Structure
///
/// The provider uses Redis hash tags `{query_id}` so all keys for a query
/// hash to the same Redis Cluster slot:
/// ```text
/// ei:{my-query}:source1:elem1   - Element data
/// ari:{my-query}:42             - Accumulator/result data
/// fqi:{my-query}                - Future queue
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
    enable_archive: bool,
}

impl GarnetIndexProvider {
    /// Create a new Garnet/Redis index provider.
    ///
    /// # Arguments
    ///
    /// * `connection_string` - Redis connection URL (e.g., "redis://localhost:6379")
    /// * `cache_size` - Optional local cache size for improved read performance
    /// * `enable_archive` - Enable archive index for point-in-time queries
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Without caching
    /// let provider = GarnetIndexProvider::new("redis://localhost:6379", None, true);
    ///
    /// // With caching (1000 element cache)
    /// let provider = GarnetIndexProvider::new("redis://localhost:6379", Some(1000), false);
    /// ```
    pub fn new(
        connection_string: impl Into<String>,
        cache_size: Option<usize>,
        enable_archive: bool,
    ) -> Self {
        Self {
            connection_string: connection_string.into(),
            cache_size,
            enable_archive,
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

    /// Check if archive is enabled.
    pub fn is_archive_enabled(&self) -> bool {
        self.enable_archive
    }
}

#[async_trait]
impl IndexBackendPlugin for GarnetIndexProvider {
    async fn create_index_set(&self, query_id: &str) -> Result<IndexSet, IndexError> {
        let client = redis::Client::open(self.connection_string.as_str())
            .map_err(IndexError::connection_failed)?;
        let connection = client
            .get_multiplexed_async_connection()
            .await
            .map_err(IndexError::connection_failed)?;

        let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
        let session_control = Arc::new(GarnetSessionControl::new(session_state.clone()));

        let element_index = Arc::new(GarnetElementIndex::new(
            query_id,
            connection.clone(),
            self.enable_archive,
            session_state.clone(),
        ));
        let result_index = Arc::new(GarnetResultIndex::new(
            query_id,
            connection.clone(),
            session_state.clone(),
        ));
        let future_queue = Arc::new(GarnetFutureQueue::new(query_id, connection, session_state));

        Ok(IndexSet {
            element_index: element_index.clone(),
            archive_index: element_index,
            result_index,
            future_queue,
            session_control,
        })
    }

    fn is_volatile(&self) -> bool {
        false // Redis/Garnet is persistent (assuming persistence is configured)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_garnet_index_provider_new() {
        let provider = GarnetIndexProvider::new("redis://localhost:6379", None, false); // DevSkim: ignore DS162092
        assert_eq!(provider.connection_string(), "redis://localhost:6379"); // DevSkim: ignore DS162092
        assert!(provider.cache_size().is_none());
        assert!(!provider.is_archive_enabled());
    }

    #[test]
    fn test_garnet_index_provider_new_with_cache() {
        let provider = GarnetIndexProvider::new("redis://localhost:6379", Some(1000), true); // DevSkim: ignore DS162092
        assert_eq!(provider.connection_string(), "redis://localhost:6379"); // DevSkim: ignore DS162092
        assert_eq!(provider.cache_size(), Some(1000));
        assert!(provider.is_archive_enabled());
    }

    #[test]
    fn test_garnet_index_provider_connection_string_from_string() {
        let provider = GarnetIndexProvider::new(String::from("redis://myhost:1234"), None, false); // DevSkim: ignore DS162092
        assert_eq!(provider.connection_string(), "redis://myhost:1234"); // DevSkim: ignore DS162092
    }

    #[test]
    fn test_garnet_index_provider_is_volatile() {
        let provider = GarnetIndexProvider::new("redis://localhost:6379", None, false); // DevSkim: ignore DS162092
        assert!(!provider.is_volatile());
    }

    #[test]
    fn test_garnet_index_provider_large_cache_size() {
        let provider = GarnetIndexProvider::new("redis://localhost:6379", Some(1_000_000), false); // DevSkim: ignore DS162092
        assert_eq!(provider.cache_size(), Some(1_000_000));
    }

    #[test]
    fn test_garnet_index_provider_zero_cache_size() {
        // Zero cache size should be valid (effectively no caching)
        let provider = GarnetIndexProvider::new("redis://localhost:6379", Some(0), false); // DevSkim: ignore DS162092
        assert_eq!(provider.cache_size(), Some(0));
    }

    #[test]
    fn test_garnet_index_provider_connection_string_with_auth() {
        // Test connection string with authentication
        let provider = GarnetIndexProvider::new("redis://:password@localhost:6379/0", None, false); // DevSkim: ignore DS137138 DS162092
        assert_eq!(
            provider.connection_string(),
            "redis://:password@localhost:6379/0" // DevSkim: ignore DS137138 DS162092
        );
    }

    #[test]
    fn test_garnet_index_provider_connection_string_with_tls() {
        // Test connection string with TLS
        let provider = GarnetIndexProvider::new("rediss://localhost:6379", None, false); // DevSkim: ignore DS162092
        assert_eq!(provider.connection_string(), "rediss://localhost:6379"); // DevSkim: ignore DS162092
    }
}
