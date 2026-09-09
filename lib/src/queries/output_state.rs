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

//! In-memory query output state with O(1) result-set operations and outbox ring buffer.
//!
//! `QueryOutputState` replaces the naive `Vec<serde_json::Value>` approach with an
//! `im::HashMap` keyed by `row_signature`, providing:
//! - O(1) insert, update, and delete operations
//! - O(1) structural-sharing clones for non-blocking snapshot reads
//! - A bounded ring buffer (`outbox`) of recent `QueryResult` emissions

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

use crate::channels::{QueryResult, ResultDiff};

/// Wire envelope pairing a row's engine-stamped `row_signature` with its data.
///
/// Used as the per-row serialization format when snapshot rows cross the FFI
/// boundary, so the canonical row identity survives instead of being dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyedSnapshotRow {
    /// The row's `row_signature` (engine-computed identity). `0` means unknown.
    pub k: u64,
    /// The row's data payload as a JSON object (the query result columns and values).
    pub v: serde_json::Value,
}

/// Default outbox capacity if not configured.
pub const DEFAULT_OUTBOX_CAPACITY: usize = 1000;

/// In-memory state tracking the live result set and recent emissions for a query.
///
/// This struct is held behind `Arc<RwLock<...>>` in `DrasiQuery`. Writers acquire
/// a write lock to apply diffs and push to the outbox. Readers (e.g., `fetch_snapshot`)
/// acquire a read lock and clone the `im::HashMap` in O(1) via structural sharing.
///
/// All fields are private to enforce invariants (sequence monotonicity, ring buffer
/// bounds). Use accessor methods for read access and `apply_diffs` /
/// `advance_sequence_and_push` for mutations.
#[derive(Debug, Clone)]
pub struct QueryOutputState {
    /// Live result set, keyed by `row_signature` for O(1) updates.
    /// Uses `im::HashMap` for O(1) structural-sharing clones (the clone itself is
    /// constant-time; access still requires the enclosing `RwLock` read lock).
    results: im::HashMap<u64, serde_json::Value>,
    /// The result sequence number the snapshot reflects.
    /// Incremented only when non-empty diffs are emitted.
    as_of_sequence: u64,
    /// Ring buffer of recent `QueryResult` emissions (bounded by `outbox_capacity`).
    /// Stored as `Arc` for zero-copy dispatch to reactions.
    outbox: VecDeque<Arc<QueryResult>>,
    /// Maximum number of entries retained in the outbox.
    outbox_capacity: usize,
}

impl QueryOutputState {
    /// Maximum allowed outbox capacity to prevent memory exhaustion from misconfiguration.
    const MAX_OUTBOX_CAPACITY: usize = 1_000_000;

    /// Create a new empty `QueryOutputState` with the given outbox capacity.
    ///
    /// A capacity of 0 is treated as 1 (at least one entry must be retainable
    /// for correct dispatch semantics). Values above `MAX_OUTBOX_CAPACITY` (1M)
    /// are clamped to prevent unbounded memory growth from misconfiguration.
    pub fn new(outbox_capacity: usize) -> Self {
        let effective_capacity = outbox_capacity.clamp(1, Self::MAX_OUTBOX_CAPACITY);
        Self {
            results: im::HashMap::new(),
            as_of_sequence: 0,
            // Pre-allocate up to 1024 slots; the deque grows automatically for larger capacities.
            outbox: VecDeque::with_capacity(effective_capacity.min(1024)),
            outbox_capacity: effective_capacity,
        }
    }

    /// Apply a set of result diffs to the live result set using O(1) HashMap operations.
    ///
    /// This does NOT increment the sequence or push to the outbox — that is done
    /// separately by the caller after constructing the `QueryResult`.
    pub fn apply_diffs(&mut self, diffs: &[ResultDiff]) {
        for diff in diffs {
            match diff {
                ResultDiff::Add {
                    data,
                    row_signature,
                } => {
                    self.results.insert(*row_signature, data.clone());
                }
                ResultDiff::Delete { row_signature, .. } => {
                    self.results.remove(row_signature);
                }
                ResultDiff::Update {
                    after,
                    row_signature,
                    ..
                } => {
                    self.results.insert(*row_signature, after.clone());
                }
                ResultDiff::Aggregation {
                    after,
                    row_signature,
                    ..
                } => {
                    // Insert/overwrite the aggregation result for this group.
                    // Note: identity-value detection (empty group removal) depends on #384
                    // and will be handled in a follow-up.
                    self.results.insert(*row_signature, after.clone());
                }
                ResultDiff::Noop => {}
            }
        }
    }

    /// Increment the sequence counter, wrap the result in an `Arc`, push to the outbox,
    /// and return the `Arc<QueryResult>` for zero-copy dispatch.
    /// Evicts the oldest entry if the outbox is at capacity.
    pub fn advance_sequence_and_push(&mut self, mut result: QueryResult) -> Arc<QueryResult> {
        self.as_of_sequence = self.as_of_sequence.saturating_add(1);
        result.sequence = self.as_of_sequence;

        let arc_result = Arc::new(result);

        if self.outbox.len() >= self.outbox_capacity {
            self.outbox.pop_front();
        }
        self.outbox.push_back(arc_result.clone());

        arc_result
    }

    /// Return the live result set as a `Vec` for backward compatibility with `get_current_results`.
    pub fn get_results_as_vec(&self) -> Vec<serde_json::Value> {
        self.results.values().cloned().collect()
    }

    /// Return the current outbox capacity.
    pub fn outbox_capacity(&self) -> usize {
        self.outbox_capacity
    }

    /// Return the current sequence number.
    pub fn as_of_sequence(&self) -> u64 {
        self.as_of_sequence
    }

    /// Return the number of entries currently in the outbox.
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Return the sequence of the earliest entry in the outbox, or `None` if empty.
    pub fn outbox_earliest_seq(&self) -> Option<u64> {
        self.outbox.front().map(|r| r.sequence)
    }

    /// Return the number of results in the live result set.
    pub fn results_len(&self) -> usize {
        self.results.len()
    }

    /// Get a result by its row signature.
    pub fn get_result(&self, row_signature: &u64) -> Option<&serde_json::Value> {
        self.results.get(row_signature)
    }

    /// Clone the live result set as an `im::HashMap` (O(1) via structural sharing).
    ///
    /// This is used by `DrasiQuery::fetch_snapshot()` to take a lightweight clone
    /// under the read lock, then build the `SnapshotResponse` outside the lock.
    pub fn clone_results(&self) -> im::HashMap<u64, serde_json::Value> {
        self.results.clone()
    }

    /// Fetch outbox entries after the given sequence number.
    ///
    /// Returns `Ok(entries)` if the requested position is available in the ring buffer,
    /// or `Err(OutboxGap)` if the position has been evicted.
    pub fn fetch_outbox_after(
        &self,
        after_sequence: u64,
    ) -> Result<Vec<Arc<QueryResult>>, OutboxGap> {
        if after_sequence >= self.as_of_sequence {
            // Caller is at or ahead of the current sequence — nothing to return
            return Ok(Vec::new());
        }

        // Find the earliest sequence in the outbox
        let earliest = self
            .outbox
            .front()
            .map(|r| r.sequence)
            .unwrap_or(self.as_of_sequence + 1);

        if after_sequence + 1 < earliest {
            return Err(OutboxGap {
                requested: after_sequence,
                earliest_available: earliest,
                latest_sequence: self.as_of_sequence,
                config_hash: 0, // Enriched by `DrasiQuery::fetch_outbox()`
            });
        }

        // Collect entries with sequence > after_sequence (cheap Arc clone)
        let entries: Vec<Arc<QueryResult>> = self
            .outbox
            .iter()
            .filter(|r| r.sequence > after_sequence)
            .cloned()
            .collect();

        Ok(entries)
    }
}

/// Error returned when the requested outbox position has been evicted.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("Outbox gap: requested after seq {requested}, but earliest available is {earliest_available} (latest: {latest_sequence})")]
pub struct OutboxGap {
    /// The sequence the caller requested (wants entries after this).
    pub requested: u64,
    /// The earliest sequence still available in the outbox.
    pub earliest_available: u64,
    /// The latest sequence in the outbox.
    pub latest_sequence: u64,
    /// The query's config hash (set by `DrasiQuery`, not by `QueryOutputState`).
    pub config_hash: u64,
}

/// Error returned by `fetch_snapshot` or `fetch_outbox` when the query is not
/// in a state that can serve the request.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum FetchError {
    /// The query finished bootstrapping but ended in a non-Running state
    /// (e.g., Error, Stopped). The snapshot/outbox may be incomplete.
    #[error("Query is not running (status: {status:?})")]
    NotRunning {
        status: crate::channels::ComponentStatus,
    },
    /// The bootstrap did not complete within the allowed timeout.
    #[error("Timed out waiting for query to finish bootstrapping")]
    TimedOut,
    /// (fetch_outbox only) The requested outbox position has been evicted.
    #[error(transparent)]
    OutboxGap(#[from] OutboxGap),
}

/// Response from `fetch_snapshot` on the Query trait.
///
/// Contains the live result set as an O(1) `im::HashMap` clone (private) and
/// exposes it via `stream()`, `to_vec()`, `len()`, and `is_empty()`.
#[derive(Debug, Clone)]
pub struct SnapshotResponse {
    /// The live result set (private — not part of the public API).
    results: im::HashMap<u64, serde_json::Value>,
    /// The sequence number this snapshot reflects.
    pub as_of_sequence: u64,
    /// The query's configuration hash at the time of the snapshot.
    pub config_hash: u64,
}

impl SnapshotResponse {
    /// Create a new `SnapshotResponse` from an `im::HashMap` clone.
    pub fn new(
        results: im::HashMap<u64, serde_json::Value>,
        as_of_sequence: u64,
        config_hash: u64,
    ) -> Self {
        Self {
            results,
            as_of_sequence,
            config_hash,
        }
    }

    /// Return an async stream of the result values.
    ///
    /// The stream yields each `serde_json::Value` from the underlying `im::HashMap`
    /// without holding any lock (the clone was taken under the read lock).
    pub fn stream(self) -> impl Stream<Item = serde_json::Value> + Send {
        tokio_stream::iter(self.results.into_iter().map(|(_, v)| v))
    }

    /// Return an async stream of `(row_signature, value)` pairs.
    ///
    /// Like [`stream`](Self::stream) but preserves each row's `row_signature`
    /// (the canonical identity) instead of dropping it.
    pub fn stream_keyed(self) -> impl Stream<Item = (u64, serde_json::Value)> + Send {
        tokio_stream::iter(self.results)
    }

    /// Collect the results into a `Vec<serde_json::Value>`.
    pub fn to_vec(&self) -> Vec<serde_json::Value> {
        self.results.values().cloned().collect()
    }

    /// Return the number of results in the snapshot.
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Return `true` if the snapshot contains no results.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }
}

/// Response from `fetch_outbox` on the Query trait.
#[derive(Debug, Clone)]
pub struct OutboxResponse {
    /// The contiguous set of `QueryResult` entries after the requested position.
    pub results: Vec<Arc<QueryResult>>,
    /// The latest sequence number in the query's output state.
    pub latest_sequence: u64,
    /// The query's configuration hash.
    pub config_hash: u64,
}

/// Streaming snapshot response for the bootstrap path.
///
/// Wraps a `Stream` of `serde_json::Value` rows plus snapshot metadata.
/// Created from either an in-process `SnapshotResponse` (via `from_snapshot()`)
/// or an FFI iterator (via the plugin SDK).
///
/// Consumers iterate with `while let Some(row) = stream.next().await { ... }`
/// or call `collect_vec()` to drain all rows into a `Vec`.
pub struct SnapshotStream {
    inner: Pin<Box<dyn Stream<Item = (u64, serde_json::Value)> + Send>>,
    /// The sequence number this snapshot reflects.
    pub as_of_sequence: u64,
    /// The query's configuration hash at the time of the snapshot.
    pub config_hash: u64,
}

impl SnapshotStream {
    /// Create a `SnapshotStream` from an in-process `SnapshotResponse`.
    pub fn from_snapshot(snapshot: SnapshotResponse) -> Self {
        let as_of_sequence = snapshot.as_of_sequence;
        let config_hash = snapshot.config_hash;
        Self {
            inner: Box::pin(snapshot.stream_keyed()),
            as_of_sequence,
            config_hash,
        }
    }

    /// Create a `SnapshotStream` from a stream of bare row values.
    ///
    /// Rows created this way have an unknown `row_signature` (stamped as `0`);
    /// use [`from_keyed_stream`](Self::from_keyed_stream) when signatures are
    /// available.
    pub fn from_stream(
        stream: impl Stream<Item = serde_json::Value> + Send + 'static,
        as_of_sequence: u64,
        config_hash: u64,
    ) -> Self {
        use tokio_stream::StreamExt;
        Self {
            inner: Box::pin(stream.map(|v| (0u64, v))),
            as_of_sequence,
            config_hash,
        }
    }

    /// Create a `SnapshotStream` from a stream of `(row_signature, value)` pairs.
    ///
    /// Prefer this over [`from_stream`](Self::from_stream) when row signatures are
    /// available; signatures let downstream consumers deduplicate and match rows
    /// by canonical identity (see `row_signature`).
    pub fn from_keyed_stream(
        stream: impl Stream<Item = (u64, serde_json::Value)> + Send + 'static,
        as_of_sequence: u64,
        config_hash: u64,
    ) -> Self {
        Self {
            inner: Box::pin(stream),
            as_of_sequence,
            config_hash,
        }
    }

    /// Collect all rows from the stream into a `Vec`, dropping signatures.
    ///
    /// Use [`collect_keyed_vec`](Self::collect_keyed_vec) to retain each row's
    /// `row_signature` alongside its data.
    pub async fn collect_vec(self) -> Vec<serde_json::Value> {
        use tokio_stream::StreamExt;
        self.inner.map(|(_, v)| v).collect().await
    }

    /// Collect all rows from the stream into a `Vec` of `(row_signature, value)` pairs.
    pub async fn collect_keyed_vec(self) -> Vec<(u64, serde_json::Value)> {
        use tokio_stream::StreamExt;
        self.inner.collect().await
    }

    /// Collect up to `limit` `(row_signature, value)` pairs, then stop pulling.
    ///
    /// Bounds peak memory when draining a potentially large snapshot into a
    /// capped consumer (e.g. a store with a per-query row limit).
    pub async fn collect_keyed_vec_capped(self, limit: usize) -> Vec<(u64, serde_json::Value)> {
        use tokio_stream::StreamExt;
        self.inner.take(limit).collect().await
    }

    /// Pull the next `(row_signature, value)` pair from the stream.
    pub async fn next_keyed(&mut self) -> Option<(u64, serde_json::Value)> {
        use tokio_stream::StreamExt;
        self.inner.next().await
    }
}

/// Yields `serde_json::Value` rows, **dropping each row's `row_signature`**.
///
/// This trait impl is retained for value-only consumers (e.g. bootstrap replay in
/// other reactions). Identity-preserving consumers must instead use the keyed
/// methods ([`next_keyed`](SnapshotStream::next_keyed) /
/// [`collect_keyed_vec`](SnapshotStream::collect_keyed_vec)); routing snapshot rows
/// through this lossy path is what previously caused the #605 deduplication bug.
impl Stream for SnapshotStream {
    type Item = serde_json::Value;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner
            .as_mut()
            .poll_next(cx)
            .map(|opt| opt.map(|(_, v)| v))
    }
}

/// Streaming outbox response for the bootstrap path.
///
/// Wraps a `Stream` of `Arc<QueryResult>` entries plus outbox metadata.
pub struct OutboxStream {
    inner: Pin<Box<dyn Stream<Item = Arc<QueryResult>> + Send>>,
    /// The latest sequence number in the query's output state.
    pub latest_sequence: u64,
    /// The query's configuration hash.
    pub config_hash: u64,
}

impl OutboxStream {
    /// Create an `OutboxStream` from an in-process `OutboxResponse`.
    pub fn from_outbox(outbox: OutboxResponse) -> Self {
        let latest_sequence = outbox.latest_sequence;
        let config_hash = outbox.config_hash;
        Self {
            inner: Box::pin(tokio_stream::iter(outbox.results)),
            latest_sequence,
            config_hash,
        }
    }

    /// Create an `OutboxStream` from an arbitrary `Stream` implementation.
    pub fn from_stream(
        stream: impl Stream<Item = Arc<QueryResult>> + Send + 'static,
        latest_sequence: u64,
        config_hash: u64,
    ) -> Self {
        Self {
            inner: Box::pin(stream),
            latest_sequence,
            config_hash,
        }
    }

    /// Collect all entries from the stream into a `Vec`.
    pub async fn collect_vec(self) -> Vec<Arc<QueryResult>> {
        use tokio_stream::StreamExt;
        self.inner.collect().await
    }
}

impl Stream for OutboxStream {
    type Item = Arc<QueryResult>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
        QueryResult::new(
            query_id.to_string(),
            0, // sequence will be set by advance_sequence_and_push
            chrono::Utc::now(),
            diffs,
            HashMap::new(),
        )
    }

    #[test]
    fn test_apply_diffs_add() {
        let mut state = QueryOutputState::new(10);
        let diffs = vec![ResultDiff::Add {
            data: serde_json::json!({"name": "Alice"}),
            row_signature: 100,
        }];
        state.apply_diffs(&diffs);

        assert_eq!(state.results.len(), 1);
        assert_eq!(
            state.results.get(&100),
            Some(&serde_json::json!({"name": "Alice"}))
        );
    }

    #[test]
    fn test_apply_diffs_delete() {
        let mut state = QueryOutputState::new(10);
        state
            .results
            .insert(100, serde_json::json!({"name": "Alice"}));

        let diffs = vec![ResultDiff::Delete {
            data: serde_json::json!({"name": "Alice"}),
            row_signature: 100,
        }];
        state.apply_diffs(&diffs);

        assert_eq!(state.results.len(), 0);
    }

    #[test]
    fn test_apply_diffs_update() {
        let mut state = QueryOutputState::new(10);
        state
            .results
            .insert(100, serde_json::json!({"name": "Alice"}));

        let diffs = vec![ResultDiff::Update {
            data: serde_json::json!({"name": "Bob"}),
            before: serde_json::json!({"name": "Alice"}),
            after: serde_json::json!({"name": "Bob"}),
            grouping_keys: None,
            row_signature: 100,
        }];
        state.apply_diffs(&diffs);

        assert_eq!(state.results.len(), 1);
        assert_eq!(
            state.results.get(&100),
            Some(&serde_json::json!({"name": "Bob"}))
        );
    }

    #[test]
    fn test_apply_diffs_aggregation() {
        let mut state = QueryOutputState::new(10);

        let diffs = vec![ResultDiff::Aggregation {
            before: None,
            after: serde_json::json!({"count": 5}),
            row_signature: 200,
        }];
        state.apply_diffs(&diffs);

        assert_eq!(state.results.len(), 1);
        assert_eq!(
            state.results.get(&200),
            Some(&serde_json::json!({"count": 5}))
        );

        // Update aggregation
        let diffs = vec![ResultDiff::Aggregation {
            before: Some(serde_json::json!({"count": 5})),
            after: serde_json::json!({"count": 10}),
            row_signature: 200,
        }];
        state.apply_diffs(&diffs);

        assert_eq!(state.results.len(), 1);
        assert_eq!(
            state.results.get(&200),
            Some(&serde_json::json!({"count": 10}))
        );
    }

    #[test]
    fn test_apply_diffs_noop() {
        let mut state = QueryOutputState::new(10);
        state
            .results
            .insert(100, serde_json::json!({"name": "Alice"}));

        let diffs = vec![ResultDiff::Noop];
        state.apply_diffs(&diffs);

        assert_eq!(state.results.len(), 1);
    }

    #[test]
    fn test_advance_sequence_and_push() {
        let mut state = QueryOutputState::new(3);

        let result = make_query_result("q1", vec![]);
        let arc = state.advance_sequence_and_push(result);
        assert_eq!(arc.sequence, 1);
        assert_eq!(state.as_of_sequence, 1);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(state.outbox.back().unwrap().sequence, 1);

        let result = make_query_result("q1", vec![]);
        let arc = state.advance_sequence_and_push(result);
        assert_eq!(arc.sequence, 2);
        assert_eq!(state.outbox.len(), 2);
    }

    #[test]
    fn test_outbox_capacity_eviction() {
        let mut state = QueryOutputState::new(3);

        for _ in 0..5 {
            let result = make_query_result("q1", vec![]);
            state.advance_sequence_and_push(result);
        }

        assert_eq!(state.outbox.len(), 3);
        assert_eq!(state.as_of_sequence, 5);
        // Oldest should be seq 3 (1 and 2 evicted)
        assert_eq!(state.outbox.front().unwrap().sequence, 3);
        assert_eq!(state.outbox.back().unwrap().sequence, 5);
    }

    #[test]
    fn test_fetch_outbox_after_caught_up() {
        let mut state = QueryOutputState::new(10);

        let result = make_query_result("q1", vec![]);
        state.advance_sequence_and_push(result);

        // Asking for entries after current sequence → empty
        let entries = state.fetch_outbox_after(1).unwrap();
        assert!(entries.is_empty());

        // Asking for entries after a future sequence → also empty
        let entries = state.fetch_outbox_after(100).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn test_fetch_outbox_after_returns_entries() {
        let mut state = QueryOutputState::new(10);

        for _ in 0..5 {
            let result = make_query_result("q1", vec![]);
            state.advance_sequence_and_push(result);
        }

        let entries = state.fetch_outbox_after(2).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sequence, 3);
        assert_eq!(entries[1].sequence, 4);
        assert_eq!(entries[2].sequence, 5);
    }

    #[test]
    fn test_fetch_outbox_after_gap_error() {
        let mut state = QueryOutputState::new(3);

        for _ in 0..5 {
            let result = make_query_result("q1", vec![]);
            state.advance_sequence_and_push(result);
        }

        // Outbox contains seq 3, 4, 5. Requesting after seq 0 → gap
        let err = state.fetch_outbox_after(0).unwrap_err();
        assert_eq!(err.requested, 0);
        assert_eq!(err.earliest_available, 3);
        assert_eq!(err.latest_sequence, 5);
        assert_eq!(err.config_hash, 0); // Default; enriched by DrasiQuery
    }

    #[test]
    fn test_get_results_as_vec() {
        let mut state = QueryOutputState::new(10);
        state.results.insert(1, serde_json::json!({"a": 1}));
        state.results.insert(2, serde_json::json!({"b": 2}));

        let vec = state.get_results_as_vec();
        assert_eq!(vec.len(), 2);
        // Order is not guaranteed from HashMap, just check both values are present
        assert!(vec.contains(&serde_json::json!({"a": 1})));
        assert!(vec.contains(&serde_json::json!({"b": 2})));
    }

    #[test]
    fn test_snapshot_clone_is_independent() {
        let mut state = QueryOutputState::new(10);
        state
            .results
            .insert(1, serde_json::json!({"name": "Alice"}));

        // Clone the results (simulating a snapshot read)
        let snapshot = state.results.clone();

        // Mutate the original
        state.results.insert(1, serde_json::json!({"name": "Bob"}));

        // Snapshot is unchanged (structural sharing)
        assert_eq!(
            snapshot.get(&1),
            Some(&serde_json::json!({"name": "Alice"}))
        );
        assert_eq!(
            state.results.get(&1),
            Some(&serde_json::json!({"name": "Bob"}))
        );
    }

    #[test]
    fn test_outbox_capacity_zero_clamped_to_one() {
        let mut state = QueryOutputState::new(0);
        // Capacity 0 is clamped to 1
        assert_eq!(state.outbox_capacity, 1);

        let result = make_query_result("q1", vec![]);
        state.advance_sequence_and_push(result);
        assert_eq!(state.outbox.len(), 1);

        // Second push evicts the first (capacity is 1)
        let result = make_query_result("q1", vec![]);
        state.advance_sequence_and_push(result);
        assert_eq!(state.outbox.len(), 1);
        assert_eq!(state.outbox.front().unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn snapshot_stream_yields_all_values() {
        use tokio_stream::StreamExt;

        let mut map = im::HashMap::new();
        map.insert(1, serde_json::json!({"id": 1}));
        map.insert(2, serde_json::json!({"id": 2}));
        map.insert(3, serde_json::json!({"id": 3}));

        let snap = SnapshotResponse::new(map, 10, 42);
        assert_eq!(snap.len(), 3);
        assert!(!snap.is_empty());

        // Consume via stream()
        let mut collected: Vec<serde_json::Value> = snap.stream().collect().await;
        collected.sort_by_key(|v| v["id"].as_u64().unwrap());
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0]["id"], 1);
        assert_eq!(collected[1]["id"], 2);
        assert_eq!(collected[2]["id"], 3);
    }

    #[tokio::test]
    async fn test_snapshot_stream_preserves_row_signatures() {
        use tokio_stream::StreamExt;

        let mut map = im::HashMap::new();
        map.insert(11u64, serde_json::json!({"id": 1}));
        map.insert(22u64, serde_json::json!({"id": 2}));

        let snap = SnapshotResponse::new(map, 5, 7);

        // stream_keyed yields (row_signature, value) pairs.
        let mut keyed: Vec<(u64, serde_json::Value)> = snap.clone().stream_keyed().collect().await;
        keyed.sort_by_key(|(sig, _)| *sig);
        assert_eq!(keyed.len(), 2);
        assert_eq!(keyed[0].0, 11);
        assert_eq!(keyed[0].1["id"], 1);
        assert_eq!(keyed[1].0, 22);

        // SnapshotStream::collect_keyed_vec preserves signatures too.
        let stream = SnapshotStream::from_snapshot(snap);
        assert_eq!(stream.as_of_sequence, 5);
        let mut via_stream = stream.collect_keyed_vec().await;
        via_stream.sort_by_key(|(sig, _)| *sig);
        assert_eq!(via_stream.len(), 2);
        assert_eq!(via_stream[0].0, 11);
        assert_eq!(via_stream[1].0, 22);
    }

    #[tokio::test]
    async fn test_snapshot_stream_from_bare_values_uses_zero_signature() {
        let stream = SnapshotStream::from_stream(
            tokio_stream::iter(vec![serde_json::json!({"id": 1})]),
            0,
            0,
        );
        let keyed = stream.collect_keyed_vec().await;
        assert_eq!(keyed.len(), 1);
        assert_eq!(
            keyed[0].0, 0,
            "bare-value stream rows have unknown signature 0"
        );
    }
}
