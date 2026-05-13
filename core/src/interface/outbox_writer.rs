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

//! Persistent outbox writer trait for reaction recovery.
//!
//! The outbox is a bounded ring buffer of serialized [`QueryResult`] entries,
//! keyed by query ID and sequence number. Reactions read the outbox on restart
//! to replay events missed while they were stopped.
//!
//! [`QueryResult`]: Used in the `lib` crate; this trait operates on serialized
//! bytes so that storage backends (RocksDB, Garnet) don't depend on `drasi-lib`.
//!
//! ## Key semantics
//!
//! - **append**: Store a result at a given sequence. The outbox is bounded by
//!   capacity; implementations may evict the oldest entries.
//! - **read_from**: Return all entries with sequence > `after_sequence`, in order.
//! - **trim_to_capacity**: Explicitly evict oldest entries beyond a limit.

use async_trait::async_trait;

use super::IndexError;

/// Persistent outbox storage for query result replay.
///
/// Implementations store serialized `QueryResult` bytes keyed by `(query_id, sequence)`.
/// The `lib` layer handles serialization/deserialization.
#[async_trait]
pub trait OutboxWriter: Send + Sync {
    /// Append a serialized query result entry.
    ///
    /// If the outbox already contains an entry at this sequence, it is overwritten.
    async fn append(
        &self,
        query_id: &str,
        sequence: u64,
        data: &[u8],
    ) -> Result<(), IndexError>;

    /// Read all entries with sequence strictly greater than `after_sequence`.
    ///
    /// Returns entries in ascending sequence order as `(sequence, data)` pairs.
    /// Returns an empty vec if no entries exist after the given sequence.
    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError>;

    /// Read the highest sequence number stored for this query.
    ///
    /// Returns `None` if the outbox is empty for this query.
    async fn read_latest_sequence(
        &self,
        query_id: &str,
    ) -> Result<Option<u64>, IndexError>;

    /// Delete all outbox entries for a query.
    ///
    /// Used during `AutoReset` recovery and reaction deprovisioning.
    async fn clear(&self, query_id: &str) -> Result<(), IndexError>;

    /// Trim the outbox to at most `capacity` entries, removing the oldest.
    ///
    /// Returns the number of entries removed. If the outbox has ≤ `capacity`
    /// entries, this is a no-op returning 0.
    async fn trim_to_capacity(
        &self,
        query_id: &str,
        capacity: usize,
    ) -> Result<usize, IndexError>;
}
