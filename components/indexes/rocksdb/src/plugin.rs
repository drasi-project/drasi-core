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
