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

//! RocksDB Index Backend Plugin
//!
//! This module provides the `RocksDbIndexProvider` which implements the
//! `IndexBackendPlugin` trait for RocksDB-based persistent storage.
//!
//! # Example
//!
//! ```ignore
//! use drasi_index_rocksdb::RocksDbIndexProvider;
//! use drasi_lib::DrasiLib;
//! use std::sync::Arc;
//!
//! let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
//! let drasi = DrasiLib::builder()
//!     .with_index_provider(Arc::new(provider))
//!     .build()?;
//! ```

use async_trait::async_trait;
use drasi_core::interface::{
    ElementArchiveIndex, ElementIndex, FutureQueue, IndexBackendPlugin, IndexError, ResultIndex,
};
use std::path::PathBuf;
use std::sync::Arc;

use crate::element_index::{RocksDbElementIndex, RocksIndexOptions};
use crate::future_queue::RocksDbFutureQueue;
use crate::result_index::RocksDbResultIndex;

/// RocksDB index backend provider.
///
/// This provider creates RocksDB-backed indexes for persistent storage.
/// Data survives restarts, so queries using this backend do not require
/// re-bootstrapping.
///
/// # Configuration
///
/// - `path`: Base directory for RocksDB data files
/// - `enable_archive`: Enable archive index for `past()` function support
/// - `direct_io`: Use direct I/O for better performance on SSDs
///
/// # Directory Structure
///
/// RocksDB creates the following directory structure:
/// ```text
/// {path}/
///   {query_id}/
///     elements/    - Element index data
///     aggregations/ - Result index data
///     fqi/         - Future queue data
/// ```
pub struct RocksDbIndexProvider {
    path: PathBuf,
    enable_archive: bool,
    direct_io: bool,
}

impl RocksDbIndexProvider {
    /// Create a new RocksDB index provider.
    ///
    /// # Arguments
    ///
    /// * `path` - Base directory for RocksDB data files
    /// * `enable_archive` - Enable archive index for point-in-time queries
    /// * `direct_io` - Use direct I/O (recommended for SSDs)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
    /// ```
    pub fn new<P: Into<PathBuf>>(path: P, enable_archive: bool, direct_io: bool) -> Self {
        Self {
            path: path.into(),
            enable_archive,
            direct_io,
        }
    }

    /// Get the configured path.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Check if archive is enabled.
    pub fn is_archive_enabled(&self) -> bool {
        self.enable_archive
    }

    /// Check if direct I/O is enabled.
    pub fn is_direct_io_enabled(&self) -> bool {
        self.direct_io
    }
}

#[async_trait]
impl IndexBackendPlugin for RocksDbIndexProvider {
    async fn create_element_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ElementIndex>, IndexError> {
        let path = self.path.to_string_lossy().to_string();
        let options = RocksIndexOptions {
            archive_enabled: self.enable_archive,
            direct_io: self.direct_io,
        };

        let index = RocksDbElementIndex::new(query_id, &path, options).map_err(|e| {
            log::error!(
                "Failed to create RocksDB element index for query '{query_id}' at path '{path}': {e}"
            );
            e
        })?;

        Ok(Arc::new(index))
    }

    async fn create_archive_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ElementArchiveIndex>, IndexError> {
        // RocksDB shares the element index and archive index in the same instance
        // The archive functionality is enabled/disabled via RocksIndexOptions
        let path = self.path.to_string_lossy().to_string();
        let options = RocksIndexOptions {
            archive_enabled: self.enable_archive,
            direct_io: self.direct_io,
        };

        let index = RocksDbElementIndex::new(query_id, &path, options).map_err(|e| {
            log::error!(
                "Failed to create RocksDB archive index for query '{query_id}' at path '{path}': {e}"
            );
            e
        })?;

        Ok(Arc::new(index))
    }

    async fn create_result_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ResultIndex>, IndexError> {
        let path = self.path.to_string_lossy().to_string();

        let index = RocksDbResultIndex::new(query_id, &path).map_err(|e| {
            log::error!(
                "Failed to create RocksDB result index for query '{query_id}' at path '{path}': {e}"
            );
            e
        })?;

        Ok(Arc::new(index))
    }

    async fn create_future_queue(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn FutureQueue>, IndexError> {
        let path = self.path.to_string_lossy().to_string();

        let queue = RocksDbFutureQueue::new(query_id, &path).map_err(|e| {
            log::error!(
                "Failed to create RocksDB future queue for query '{query_id}' at path '{path}': {e}"
            );
            e
        })?;

        Ok(Arc::new(queue))
    }

    fn is_volatile(&self) -> bool {
        false // RocksDB is persistent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_rocksdb_index_provider_new() {
        let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
        assert_eq!(provider.path(), &PathBuf::from("/data/drasi"));
        assert!(provider.is_archive_enabled());
        assert!(!provider.is_direct_io_enabled());
    }

    #[test]
    fn test_rocksdb_index_provider_new_from_pathbuf() {
        let path = PathBuf::from("/var/lib/drasi");
        let provider = RocksDbIndexProvider::new(path, false, true);
        assert_eq!(provider.path(), &PathBuf::from("/var/lib/drasi"));
        assert!(!provider.is_archive_enabled());
        assert!(provider.is_direct_io_enabled());
    }

    #[test]
    fn test_rocksdb_index_provider_is_volatile() {
        let provider = RocksDbIndexProvider::new("/tmp/test", false, false);
        assert!(!provider.is_volatile());
    }

    #[test]
    fn test_rocksdb_index_provider_all_options_enabled() {
        let provider = RocksDbIndexProvider::new("/data/drasi", true, true);
        assert!(provider.is_archive_enabled());
        assert!(provider.is_direct_io_enabled());
    }

    #[test]
    fn test_rocksdb_index_provider_all_options_disabled() {
        let provider = RocksDbIndexProvider::new("/data/drasi", false, false);
        assert!(!provider.is_archive_enabled());
        assert!(!provider.is_direct_io_enabled());
    }

    #[tokio::test]
    async fn test_rocksdb_create_element_index() {
        let temp_dir = TempDir::new().unwrap();
        let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);

        let result = provider.create_element_index("test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_create_archive_index() {
        let temp_dir = TempDir::new().unwrap();
        let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);

        let result = provider.create_archive_index("test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_create_result_index() {
        let temp_dir = TempDir::new().unwrap();
        let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);

        let result = provider.create_result_index("test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_create_future_queue() {
        let temp_dir = TempDir::new().unwrap();
        let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);

        let result = provider.create_future_queue("test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_create_all_indexes() {
        let temp_dir = TempDir::new().unwrap();
        let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);

        // Create all index types with unique query IDs to avoid conflicts
        // since RocksDB creates separate directories per query
        let element_result = provider.create_element_index("query_element").await;
        let archive_result = provider.create_archive_index("query_archive").await;
        let result_result = provider.create_result_index("query_result").await;
        let queue_result = provider.create_future_queue("query_queue").await;

        assert!(element_result.is_ok());
        assert!(archive_result.is_ok());
        assert!(result_result.is_ok());
        assert!(queue_result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_multiple_queries() {
        let temp_dir = TempDir::new().unwrap();
        let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);

        // Create indexes for multiple queries
        let result1 = provider.create_element_index("query1").await;
        let result2 = provider.create_element_index("query2").await;
        let result3 = provider.create_element_index("query3").await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert!(result3.is_ok());
    }
}
