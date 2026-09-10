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
//! let provider = RocksDbIndexProvider::new("/data/drasi", true, false)
//!     .with_memory_budget_bytes(512 << 20)?;
//! let drasi = DrasiLib::builder()
//!     .with_index_provider("rocksdb", Arc::new(provider))
//!     .build()?;
//! ```

use crate::IndexDb;
use async_trait::async_trait;
use drasi_core::interface::{CreatedIndexes, IndexBackendPlugin, IndexError, IndexSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::checkpoint::{self, RocksDbCheckpointStore};
use crate::element_index::{self, RocksDbElementIndex};
use crate::future_queue::{self, RocksDbFutureQueue};
use crate::live_results::{self, RocksDbLiveResultsWriter};
use crate::outbox::{self, RocksDbOutboxWriter};
use crate::result_index::{self, RocksDbResultIndex};
use crate::{
    RocksDbMemoryBudget, RocksDbMemoryBudgetError, RocksDbSessionControl, RocksDbSessionState,
    RocksIndexOptions,
};

/// Open a unified RocksDB database with all column families needed for a query.
///
/// This creates a single `IndexDb` instance containing all
/// column families for element index, result index, and future queue.
///
/// # Arguments
///
/// * `path` - Base directory for RocksDB data files
/// * `query_id` - Unique identifier for the query
/// * `options` - RocksDB index options and shared memory budget
///
/// # Directory Structure
///
/// Data is stored at `{path}/{query_id}/` (single unified directory).
pub fn open_unified_db(
    path: &str,
    query_id: &str,
    options: &RocksIndexOptions,
) -> Result<Arc<IndexDb>, IndexError> {
    // `query_id` is used directly as a directory name under `path`. Reject values
    // that could escape the base directory or otherwise misbehave as a path
    // segment (separators, parent/current-dir references, NUL, or empty).
    if query_id.is_empty()
        || query_id == "."
        || query_id == ".."
        || query_id.contains('/')
        || query_id.contains('\\')
        || query_id.contains('\0')
    {
        return Err(IndexError::other(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("Invalid query_id '{query_id}' for RocksDB path segment"),
        )));
    }

    let block_cache = options.memory_budget().block_cache();
    let mut db_opts = rocksdb::Options::default();
    db_opts.create_if_missing(true);
    db_opts.create_missing_column_families(true);
    db_opts.set_write_buffer_manager(options.memory_budget().write_buffer_manager());
    db_opts.set_use_direct_reads(options.direct_io());
    db_opts.set_use_direct_io_for_flush_and_compaction(options.direct_io());

    let db_path = PathBuf::from(path).join(query_id);
    let db_path = match db_path.to_str() {
        Some(p) => p.to_string(),
        None => return Err(IndexError::NotSupported),
    };

    let mut cfs = element_index::element_cf_descriptors(options);
    cfs.extend(result_index::result_cf_descriptors(options));
    cfs.extend(future_queue::future_queue_cf_descriptors(options));
    cfs.push(checkpoint::stream_state_cf_descriptor(options));
    cfs.push(outbox::outbox_cf_descriptor(options));
    cfs.push(live_results::live_results_cf_descriptor(options));

    // The default CF is not covered by db_opts: rust-rocksdb opens it with
    // fresh Options unless a descriptor is supplied, so it goes through the
    // same shared-cache, sizing, and history policies as every other CF.
    let default_cf_opts = crate::cf_options::base_cf_options(block_cache);
    cfs.push(crate::sizing::descriptor(
        rocksdb::DEFAULT_COLUMN_FAMILY_NAME,
        default_cf_opts,
        options,
    ));

    let txn_db_opts = rocksdb::TransactionDBOptions::default();
    let db = IndexDb::open_cf_descriptors(&db_opts, &txn_db_opts, db_path, cfs)
        .map_err(IndexError::other)?;
    Ok(Arc::new(db))
}

/// RocksDB index backend provider.
///
/// This provider creates RocksDB-backed indexes for persistent storage.
/// All indexes for a query share a single `IndexDb` instance,
/// reducing resource overhead and enabling cross-index atomic transactions.
///
/// # Configuration
///
/// - `path`: Base directory for RocksDB data files
/// - `enable_archive`: Enable archive index for `past()` function support
/// - `direct_io`: Use direct I/O for better performance on SSDs
/// - `memory_budget`: Shared block cache and write-buffer manager
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
    memory_budget: RocksDbMemoryBudget,
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
            memory_budget: RocksDbMemoryBudget::default(),
        }
    }

    /// Set the provider-wide combined memory budget using the standard policy.
    ///
    /// Half of the capacity is available to memtables. Memtable reservations
    /// are charged to the same cache, so the two capacities are not additive.
    /// Per-CF flush thresholds are derived from the memtable budget when each
    /// query DB opens. They are not reservations: aggregate pressure across
    /// query DBs can make the shared manager flush before those thresholds.
    pub fn with_memory_budget_bytes(
        mut self,
        total_budget_bytes: usize,
    ) -> Result<Self, RocksDbMemoryBudgetError> {
        self.memory_budget = RocksDbMemoryBudget::from_total_budget_bytes(total_budget_bytes)?;
        Ok(self)
    }

    /// Inject provider-wide memory resources using a custom policy.
    ///
    /// This expert API supports custom cache/write-buffer splits, stall
    /// behavior, and sharing one budget across multiple providers. Prefer
    /// [`Self::with_memory_budget_bytes`] for standard configuration.
    pub fn with_memory_budget(mut self, memory_budget: RocksDbMemoryBudget) -> Self {
        self.memory_budget = memory_budget;
        self
    }

    /// Shared memory resources used by this provider's query databases.
    pub fn memory_budget(&self) -> &RocksDbMemoryBudget {
        &self.memory_budget
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
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        let path = self.path.to_string_lossy().to_string();
        let options = RocksIndexOptions::new(
            self.enable_archive,
            self.direct_io,
            self.memory_budget.clone(),
        );

        let db = open_unified_db(&path, query_id, &options).map_err(|e| {
            log::error!(
                "Failed to open unified RocksDB for query '{query_id}' at path '{path}': {e}"
            );
            e
        })?;

        let session_state = Arc::new(RocksDbSessionState::new(db.clone()));
        let session_control = Arc::new(RocksDbSessionControl::new(session_state.clone()));

        let element_index = Arc::new(RocksDbElementIndex::new(
            db.clone(),
            options.clone(),
            session_state.clone(),
        ));
        let result_index = Arc::new(RocksDbResultIndex::new(
            db.clone(),
            session_state.clone(),
            options.clone(),
        ));
        let future_queue = Arc::new(RocksDbFutureQueue::new(
            db.clone(),
            session_state.clone(),
            options,
        ));
        let checkpoint_store = Arc::new(RocksDbCheckpointStore::new(
            db.clone(),
            session_state.clone(),
        ));
        let outbox_writer = Arc::new(RocksDbOutboxWriter::new(db.clone(), session_state.clone()));
        let live_results_writer = Arc::new(RocksDbLiveResultsWriter::new(db, session_state));

        Ok(CreatedIndexes {
            set: IndexSet {
                element_index: element_index.clone(),
                archive_index: element_index,
                result_index,
                future_queue,
                session_control,
            },
            checkpoint_store: Some(checkpoint_store),
            outbox_writer: Some(outbox_writer),
            live_results_writer: Some(live_results_writer),
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
        assert_eq!(
            provider
                .memory_budget()
                .write_buffer_manager()
                .get_buffer_size(),
            crate::DEFAULT_WRITE_BUFFER_BUDGET_BYTES
        );
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
    fn test_rocksdb_index_provider_total_memory_budget() {
        let provider = RocksDbIndexProvider::new("/data/drasi", false, false)
            .with_memory_budget_bytes(64 * 1024 * 1024)
            .expect("valid total budget");

        assert_eq!(
            provider.memory_budget().block_cache_capacity_bytes(),
            64 * 1024 * 1024
        );
        assert_eq!(
            provider
                .memory_budget()
                .write_buffer_manager()
                .get_buffer_size(),
            32 * 1024 * 1024
        );
    }

    #[test]
    fn test_rocksdb_index_provider_rejects_invalid_total_memory_budget() {
        assert!(matches!(
            RocksDbIndexProvider::new("/data/drasi", false, false).with_memory_budget_bytes(1),
            Err(RocksDbMemoryBudgetError::ZeroWriteBufferBudget)
        ));
    }

    #[test]
    fn test_rocksdb_index_provider_custom_memory_budget() {
        let budget = RocksDbMemoryBudget::new(32 * 1024 * 1024, 8 * 1024 * 1024, false)
            .expect("valid budget");
        let provider =
            RocksDbIndexProvider::new("/data/drasi", false, false).with_memory_budget(budget);

        assert_eq!(
            provider.memory_budget().block_cache_capacity_bytes(),
            32 * 1024 * 1024
        );
        assert_eq!(
            provider
                .memory_budget()
                .write_buffer_manager()
                .get_buffer_size(),
            8 * 1024 * 1024
        );
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

        let result = provider.create_indexes("test_query").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_rocksdb_create_index_set_multiple_queries() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let provider = RocksDbIndexProvider::new(temp_dir.path(), false, false);

        let result1 = provider.create_indexes("query1").await;
        let result2 = provider.create_indexes("query2").await;
        let result3 = provider.create_indexes("query3").await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert!(result3.is_ok());
    }

    #[test]
    fn test_open_unified_db() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let options = RocksIndexOptions::new(true, false, RocksDbMemoryBudget::default());

        let path = temp_dir.path().to_string_lossy().to_string();
        let result = open_unified_db(&path, "test_query", &options);
        assert!(result.is_ok());
    }

    #[test]
    fn test_open_unified_db_rejects_unsafe_query_id() {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let options = RocksIndexOptions::new(true, false, RocksDbMemoryBudget::default());
        let path = temp_dir.path().to_string_lossy().to_string();

        for bad in ["", ".", "..", "a/b", "../escape", "a\\b", "with\0nul"] {
            let result = open_unified_db(&path, bad, &options);
            assert!(
                result.is_err(),
                "query_id '{bad}' should be rejected as a path segment"
            );
        }
    }
}
