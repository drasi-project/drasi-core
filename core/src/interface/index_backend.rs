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

//! Index Backend Plugin Trait
//!
//! This module defines the `IndexBackendPlugin` trait that external index backends
//! (like RocksDB, Garnet/Redis) must implement to integrate with Drasi.
//!
//! # Architecture
//!
//! The index plugin system follows pure dependency inversion:
//! - **Core** provides index traits (`ElementIndex`, `ResultIndex`, etc.) and a default
//!   in-memory implementation
//! - **Lib** uses this plugin trait but has no knowledge of specific implementations
//! - **External plugins** (in `components/indexes/`) implement this trait
//! - **Applications** optionally inject plugins; if none provided, the in-memory default is used

use async_trait::async_trait;
use std::fmt;
use std::sync::Arc;

use super::{
    ElementArchiveIndex, ElementIndex, FutureQueue, IndexError, ResultIndex, SessionControl,
};

/// Set of indexes for a query.
///
/// Groups the index types and session control needed for query evaluation into
/// a single unit.
/// This enables backends to create all indexes from a shared underlying resource
/// (e.g., a single RocksDB instance or Redis connection).
pub struct IndexSet {
    /// Element index for storing graph elements
    pub element_index: Arc<dyn ElementIndex>,
    /// Archive index for storing historical elements (for past() function)
    pub archive_index: Arc<dyn ElementArchiveIndex>,
    /// Result index for storing query results
    pub result_index: Arc<dyn ResultIndex>,
    /// Future queue for temporal queries
    pub future_queue: Arc<dyn FutureQueue>,
    /// Session control for atomic transaction lifecycle
    pub session_control: Arc<dyn SessionControl>,
}

impl fmt::Debug for IndexSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexSet")
            .field("element_index", &"<trait object>")
            .field("archive_index", &"<trait object>")
            .field("result_index", &"<trait object>")
            .field("future_queue", &"<trait object>")
            .field("session_control", &"<trait object>")
            .finish()
    }
}

/// Plugin trait for external index storage backends.
///
/// Each storage backend (RocksDB, Garnet, etc.) implements this trait to provide
/// all index types needed for query evaluation from a single shared backend instance.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow use across async tasks.
///
/// # Example
///
/// ```ignore
/// use drasi_core::interface::{IndexBackendPlugin, IndexSet};
///
/// pub struct MyIndexProvider {
///     // configuration fields
/// }
///
/// #[async_trait]
/// impl IndexBackendPlugin for MyIndexProvider {
///     async fn create_index_set(&self, query_id: &str) -> Result<IndexSet, IndexError> {
///         // Create and return all indexes from a shared backend instance
///     }
///     fn is_volatile(&self) -> bool { false }
/// }
/// ```
#[async_trait]
pub trait IndexBackendPlugin: Send + Sync {
    /// Create all indexes for a query from a single shared backend instance.
    ///
    /// This method creates the element index, archive index, result index,
    /// future queue, and session control backed by a shared storage resource
    /// (e.g., a single RocksDB database or Redis connection). This reduces
    /// resource overhead and enables cross-index atomic transactions.
    async fn create_index_set(&self, query_id: &str) -> Result<IndexSet, IndexError>;

    /// Returns true if this backend is volatile (data lost on restart).
    ///
    /// Volatile backends (like in-memory) require re-bootstrapping after restart,
    /// while persistent backends (like RocksDB) retain data.
    fn is_volatile(&self) -> bool;
}
