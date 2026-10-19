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

//! Query-result exchange types used by the runtime, plugins and snapshot readers.
//! Query execution and result-state ownership live in the computation runtime.

use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_stream::Stream;

use crate::channels::QueryResult;

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

/// Mix query identity hash with output generation.
///
/// Generation 0 returns `config_hash` unchanged so existing reaction
/// checkpoints keep matching. A wipe/rebuild changes the value reactions
/// compare, so they apply recovery policy instead of skipping 1..=N.
pub fn output_epoch_hash(config_hash: u64, generation: u64) -> u64 {
    config_hash.wrapping_add(generation.wrapping_mul(0x9E3779B97F4A7C15))
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
    /// The query's configuration hash, set by its processing owner.
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
    /// Output generation at the time of the snapshot. `0` until a wipe/rebuild.
    pub output_generation: u64,
}

/// A consistent initial result set and changes strictly after its sequence.
///
/// Dropping the stream unregisters this connection-local reader. Neither the
/// snapshot nor the reader is a durable consumer checkpoint.
pub struct QueryResultSubscription {
    pub snapshot: SnapshotResponse,
    pub stream: futures::stream::BoxStream<'static, crate::error::Result<Arc<QueryResult>>>,
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
            output_generation: 0,
        }
    }

    /// Set the output generation carried with this snapshot.
    pub fn with_output_generation(mut self, generation: u64) -> Self {
        self.output_generation = generation;
        self
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
    /// Output generation. `0` until a wipe/rebuild.
    pub output_generation: u64,
}

impl OutboxResponse {
    pub(crate) fn is_contiguous_after(&self, after: u64) -> bool {
        let mut last = after;
        self.results.iter().all(|result| {
            let next = last.checked_add(1) == Some(result.sequence);
            last = result.sequence;
            next
        }) && last == self.latest_sequence
    }
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

    #[test]
    fn output_epoch_hash_generation_zero_is_identity() {
        assert_eq!(output_epoch_hash(42, 0), 42);
        assert_ne!(output_epoch_hash(42, 1), 42);
    }
}
