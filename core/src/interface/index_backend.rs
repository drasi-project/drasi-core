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
use std::sync::Arc;

use super::{ElementArchiveIndex, ElementIndex, FutureQueue, IndexError, ResultIndex};

/// Plugin trait for external index storage backends.
///
/// Each storage backend (RocksDB, Garnet, etc.) implements this trait to provide
/// the four index types needed for query evaluation.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync` to allow use across async tasks.
///
/// # Example
///
/// ```ignore
/// use drasi_core::interface::IndexBackendPlugin;
///
/// pub struct MyIndexProvider {
///     // configuration fields
/// }
///
/// #[async_trait]
/// impl IndexBackendPlugin for MyIndexProvider {
///     async fn create_element_index(&self, query_id: &str) -> Result<Arc<dyn ElementIndex>, IndexError> {
///         // Create and return your element index implementation
///     }
///     // ... other methods
/// }
/// ```
#[async_trait]
pub trait IndexBackendPlugin: Send + Sync {
    /// Create an ElementIndex instance for the given query.
    ///
    /// The element index manages the current graph elements (nodes and relationships)
    /// for a specific query. Each query gets its own isolated index instance.
    async fn create_element_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ElementIndex>, IndexError>;

    /// Create an ElementArchiveIndex instance for the given query.
    ///
    /// The archive index supports point-in-time queries and version history,
    /// enabling the `past()` function in Cypher queries.
    async fn create_archive_index(
        &self,
        query_id: &str,
    ) -> Result<Arc<dyn ElementArchiveIndex>, IndexError>;

    /// Create a ResultIndex instance for the given query.
    ///
    /// The result index stores accumulated query results and aggregations,
    /// supporting efficient incremental computation.
    async fn create_result_index(&self, query_id: &str)
        -> Result<Arc<dyn ResultIndex>, IndexError>;

    /// Create a FutureQueue instance for the given query.
    ///
    /// The future queue manages temporal queries and scheduled operations,
    /// enabling time-based query features.
    async fn create_future_queue(&self, query_id: &str)
        -> Result<Arc<dyn FutureQueue>, IndexError>;

    /// Returns true if this backend is volatile (data lost on restart).
    ///
    /// Volatile backends (like in-memory) require re-bootstrapping after restart,
    /// while persistent backends (like RocksDB) retain data.
    fn is_volatile(&self) -> bool;
}
