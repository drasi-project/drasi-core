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
//! - **append**: Store a result at a given sequence. Does not evict.
//! - **read_from**: Return all entries with sequence > `after_sequence`, in order.
//! - **append_and_trim**: Append then evict below `retain_from`. Durable output
//!   uses this so the ring cannot grow without a matching trim.
//! - **trim_before**: Delete entries with sequence `< retain_from`.
//! - **trim_to_capacity**: Count-based eviction for the default `trim_before`
//!   and for tests. Not used on the durable output path.

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
    /// This does not evict older entries. Durable output uses
    /// [`append_and_trim`](Self::append_and_trim) so the ring stays bounded.
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError>;

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
    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError>;

    /// Delete all outbox entries for a query.
    ///
    /// Used during `AutoReset` recovery and reaction deprovisioning.
    async fn clear(&self, query_id: &str) -> Result<(), IndexError>;

    /// Append `data` at `sequence`, then delete entries below `retain_from`.
    ///
    /// Durable output uses this so append cannot commit without the matching
    /// eviction. The default calls [`append`](Self::append) then
    /// [`trim_before`](Self::trim_before).
    async fn append_and_trim(
        &self,
        query_id: &str,
        sequence: u64,
        data: &[u8],
        retain_from: u64,
    ) -> Result<usize, IndexError> {
        self.append(query_id, sequence, data).await?;
        self.trim_before(query_id, retain_from).await
    }

    /// Delete outbox entries with sequence strictly less than `retain_from`.
    ///
    /// Returns the number of entries removed. When a session is active,
    /// deletes are staged in that session so they commit atomically with a
    /// preceding [`append`](Self::append).
    ///
    /// The default implementation keeps `[retain_from, latest]` by calling
    /// [`trim_to_capacity`](Self::trim_to_capacity). It is not session-atomic.
    /// Backends that join an outer transaction must override this.
    async fn trim_before(&self, query_id: &str, retain_from: u64) -> Result<usize, IndexError> {
        let Some(latest) = self.read_latest_sequence(query_id).await? else {
            return Ok(0);
        };
        if latest < retain_from {
            return self.trim_to_capacity(query_id, 0).await;
        }
        let keep = usize::try_from(latest.saturating_sub(retain_from).saturating_add(1))
            .unwrap_or(usize::MAX);
        self.trim_to_capacity(query_id, keep).await
    }

    /// Trim the outbox to at most `capacity` entries, removing the oldest.
    ///
    /// Count-based eviction for the default [`trim_before`](Self::trim_before)
    /// and for tests. Durable output uses sequence-based
    /// [`append_and_trim`](Self::append_and_trim) instead.
    ///
    /// Returns the number of entries removed. If the outbox has ≤ `capacity`
    /// entries, this is a no-op returning 0. When a session is active, deletes
    /// are staged in that session.
    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError>;
}
