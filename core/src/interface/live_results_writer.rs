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

//! Persistent live results writer trait for reaction recovery.
//!
//! The live results store maintains a snapshot of the current query result set.
//! Each row is keyed by its `row_signature` (a stable u64 hash). On restart,
//! reactions can read the full snapshot to rebuild their downstream state without
//! re-processing the entire event history.
//!
//! ## Key semantics
//!
//! - **apply_mutations**: Atomic batch of upserts (signature → data) and deletes
//!   (signature → None). Corresponds to the `ResultDiff` entries in a `QueryResult`.
//! - **read_snapshot**: Returns all currently live rows.
//! - **clear**: Removes all rows (used during `AutoReset` recovery).

use async_trait::async_trait;

use super::IndexError;

/// A single row mutation: upsert if data is `Some`, delete if `None`.
#[derive(Debug, Clone)]
pub struct RowMutation<'a> {
    /// Stable row identifier (hash of the row's key columns).
    pub row_signature: u64,
    /// Serialized row data for upsert, or `None` for delete.
    pub data: Option<&'a [u8]>,
}

/// Persistent live results storage for query snapshot recovery.
///
/// Implementations store serialized row data keyed by `(query_id, row_signature)`.
/// The `lib` layer handles serialization/deserialization of row data.
#[async_trait]
pub trait LiveResultsWriter: Send + Sync {
    /// Apply a batch of row mutations atomically.
    ///
    /// - `data = Some(bytes)`: Upsert the row (insert or replace).
    /// - `data = None`: Delete the row if it exists.
    ///
    /// Implementations should apply all mutations in a single batch/transaction
    /// when possible for efficiency.
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError>;

    /// Read all live rows for a query.
    ///
    /// Returns `(row_signature, serialized_data)` pairs in unspecified order.
    /// Returns an empty vec if no rows exist for this query.
    async fn read_snapshot(
        &self,
        query_id: &str,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError>;

    /// Delete all live rows for a query.
    ///
    /// Used during `AutoReset` recovery and reaction deprovisioning.
    async fn clear(&self, query_id: &str) -> Result<(), IndexError>;

    /// Count the number of live rows for a query.
    ///
    /// Useful for diagnostics and capacity monitoring.
    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError>;
}
