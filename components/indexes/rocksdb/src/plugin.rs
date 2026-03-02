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
use drasi_core::interface::{IndexBackendPlugin, IndexError, IndexSet, NoOpSessionControl};
use rocksdb::{OptimisticTransactionDB, Options};
use std::path::PathBuf;
use std::sync::Arc;

use crate::element_index::{self, RocksDbElementIndex, RocksIndexOptions};
use crate::future_queue::{self, RocksDbFutureQueue};
use crate::result_index::{self, RocksDbResultIndex};

/// Open a unified RocksDB database with all column families needed for a query.
///
/// This creates a single `OptimisticTransactionDB` instance containing all
/// column families for element index, result index, and future queue.
///
/// # Arguments
///
/// * `path` - Base directory for RocksDB data files
/// * `query_id` - Unique identifier for the query
/// * `options` - RocksDB index options (archive, direct I/O)
///
/// # Directory Structure
///
/// Data is stored at `{path}/{query_id}/` (single unified directory).
pub fn open_unified_db(
    path: &str,
    query_id: &str,
    options: &RocksIndexOptions,
) -> Result<Arc<OptimisticTransactionDB>, IndexError> {
    let mut db_opts = Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);
    db_opts.set_db_write_buffer_size(128 * 1024 * 1024);
    db_opts.set_use_direct_reads(options.direct_io);
    db_opts.set_use_direct_io_for_flush_and_compaction(options.direct_io);

    let db_path = PathBuf::from(path).join(query_id);
    let db_path = match db_path.to_str() {
        Some(p) => p.to_string(),
        None => return Err(IndexError::NotSupported),
    };

    let mut cfs = element_index::element_cf_descriptors(options);
    cfs.extend(result_index::result_cf_descriptors());
    cfs.extend(future_queue::future_queue_cf_descriptors());

    let db = OptimisticTransactionDB::open_cf_descriptors(&db_opts, db_path, cfs)
        .map_err(IndexError::other)?;
    Ok(Arc::new(db))
}

/// RocksDB index backend provider.
///
/// This provider creates RocksDB-backed indexes for persistent storage.
/// All indexes for a query share a single `OptimisticTransactionDB` instance,
/// reducing resource overhead and enabling cross-index atomic transactions.
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
///   {query_id}/   - Single unified database with all column families
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
    async fn create_index_set(&self, query_id: &str) -> Result<IndexSet, IndexError> {
        let path = self.path.to_string_lossy().to_string();
        let options = RocksIndexOptions {
            archive_enabled: self.enable_archive,
            direct_io: self.direct_io,
        };

        let db = open_unified_db(&path, query_id, &options).map_err(|e| {
            log::error!(
                "Failed to open unified RocksDB for query '{query_id}' at path '{path}': {e}"
            );
            e
        })?;

        let element_index = Arc::new(RocksDbElementIndex::new(db.clone(), options));
        let result_index = Arc::new(RocksDbResultIndex::new(db.clone()));
        let future_queue = Arc::new(RocksDbFutureQueue::new(db));

        Ok(IndexSet {
            element_index: element_index.clone(),
            archive_index: element_index,
            result_index,
            future_queue,
            session_control: Arc::new(NoOpSessionControl),
        })
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
    async fn test_rocksdb_create_index_set() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);

        let result = provider.create_index_set("test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_create_index_set_multiple_queries() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);

        let result1 = provider.create_index_set("query1").await;
        let result2 = provider.create_index_set("query2").await;
        let result3 = provider.create_index_set("query3").await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert!(result3.is_ok());
    }

    #[test]
    fn test_open_unified_db() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let options = RocksIndexOptions {
            archive_enabled: true,
            direct_io: false,
        };

        let path = temp_dir.path().to_string_lossy().to_string();
        let result = open_unified_db(&path, "test_query", &options);
        assert!(result.is_ok());
    }
}
