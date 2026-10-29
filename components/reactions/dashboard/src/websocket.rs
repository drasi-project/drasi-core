// Copyright 2026 The Drasi Authors.
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

//! WebSocket hub and protocol handlers for dashboard reactions.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use drasi_lib::channels::{QueryResult, ResultDiff};
use futures::{stream::SplitSink, SinkExt, StreamExt};
use log::{debug, warn};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, watch, RwLock};

/// Serialize a `row_signature` (`u64`) as a JSON **string**.
///
/// JavaScript `Number` only represents integers exactly up to 2^53−1, but
/// `row_signature` is a full 64-bit hash. Sending it as a string preserves all
/// 64 bits so the browser can compare identities without precision loss.
fn serialize_row_signature<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// Deserialize a `row_signature` from either a JSON string or number.
///
/// Strings are the canonical form (see [`serialize_row_signature`]); numbers are
/// accepted for resilience.
fn deserialize_row_signature<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    use serde::de::Error as _;
    match Value::deserialize(deserializer)? {
        Value::String(s) => s.parse::<u64>().map_err(D::Error::custom),
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| D::Error::custom("row_signature is not a u64")),
        Value::Null => Ok(0),
        other => Err(D::Error::custom(format!(
            "unexpected row_signature type: {other}"
        ))),
    }
}

/// Supported client-to-server WebSocket messages.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Subscribe { query_ids: Vec<String> },
    Unsubscribe { query_ids: Vec<String> },
}

/// Result item emitted to dashboard clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebSocketResultDiff {
    pub op: String,
    /// The row's engine-stamped `row_signature` — the canonical row identity.
    /// Serialized as `k` (a JSON string to preserve all 64 bits in the browser);
    /// `0` means unknown (clients fall back to id/equality).
    #[serde(
        rename = "k",
        default,
        serialize_with = "serialize_row_signature",
        deserialize_with = "deserialize_row_signature"
    )]
    pub row_signature: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grouping_keys: Option<Vec<String>>,
}

/// Query result payload sent to clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryResultPayload {
    pub query_id: String,
    #[serde(
        serialize_with = "serialize_row_signature",
        deserialize_with = "deserialize_row_signature"
    )]
    pub revision: u64,
    pub timestamp: i64,
    pub results: Vec<WebSocketResultDiff>,
}

/// Server-to-client WebSocket messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    QueryResult {
        query_id: String,
        #[serde(
            serialize_with = "serialize_row_signature",
            deserialize_with = "deserialize_row_signature"
        )]
        revision: u64,
        timestamp: i64,
        results: Vec<WebSocketResultDiff>,
    },
    QuerySnapshot {
        query_id: String,
        #[serde(
            serialize_with = "serialize_row_signature",
            deserialize_with = "deserialize_row_signature"
        )]
        revision: u64,
        rows: Vec<SnapshotRow>,
    },
    Heartbeat {
        ts: i64,
    },
}

/// Internal broadcast messages for all websocket listeners.
#[derive(Debug, Clone)]
pub enum HubMessage {
    QueryResult(QueryResultPayload),
    Heartbeat { ts: i64 },
}

impl HubMessage {
    fn into_server_message(self) -> ServerMessage {
        match self {
            HubMessage::QueryResult(payload) => ServerMessage::QueryResult {
                query_id: payload.query_id,
                revision: payload.revision,
                timestamp: payload.timestamp,
                results: payload.results,
            },
            HubMessage::Heartbeat { ts } => ServerMessage::Heartbeat { ts },
        }
    }

    /// Convert Drasi query results to websocket payload.
    pub fn from_query_result(query_result: &QueryResult, revision: u64) -> Self {
        let results = query_result
            .results
            .iter()
            .map(|result_diff| match result_diff {
                ResultDiff::Add {
                    data,
                    row_signature,
                } => WebSocketResultDiff {
                    op: "add".to_string(),
                    row_signature: *row_signature,
                    data: Some(data.clone()),
                    before: None,
                    after: None,
                    grouping_keys: None,
                },
                ResultDiff::Delete {
                    data,
                    row_signature,
                } => WebSocketResultDiff {
                    op: "delete".to_string(),
                    row_signature: *row_signature,
                    data: Some(data.clone()),
                    before: None,
                    after: None,
                    grouping_keys: None,
                },
                ResultDiff::Update {
                    data,
                    before,
                    after,
                    grouping_keys,
                    row_signature,
                } => WebSocketResultDiff {
                    op: "update".to_string(),
                    row_signature: *row_signature,
                    data: Some(data.clone()),
                    before: Some(before.clone()),
                    after: Some(after.clone()),
                    grouping_keys: grouping_keys.clone(),
                },
                ResultDiff::Aggregation {
                    before,
                    after,
                    row_signature,
                } => WebSocketResultDiff {
                    op: "aggregation".to_string(),
                    row_signature: *row_signature,
                    data: None,
                    before: before.clone(),
                    after: Some(after.clone()),
                    grouping_keys: None,
                },
                ResultDiff::Noop => WebSocketResultDiff {
                    op: "noop".to_string(),
                    row_signature: 0,
                    data: None,
                    before: None,
                    after: None,
                    grouping_keys: None,
                },
            })
            .collect();

        HubMessage::QueryResult(QueryResultPayload {
            query_id: query_result.query_id.clone(),
            revision,
            timestamp: query_result.timestamp.timestamp_millis(),
            results,
        })
    }
}

/// Broadcast hub for websocket subscribers.
#[derive(Clone)]
pub struct WebSocketHub {
    sender: broadcast::Sender<HubMessage>,
}

impl WebSocketHub {
    pub fn new(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HubMessage> {
        self.sender.subscribe()
    }

    fn broadcast_query_result(&self, query_result: &QueryResult, revision: u64) {
        if let Err(err) = self
            .sender
            .send(HubMessage::from_query_result(query_result, revision))
        {
            debug!("dashboard websocket has no active listeners for query result: {err}");
        }
    }

    pub fn broadcast_heartbeat(&self, timestamp_ms: i64) {
        if let Err(err) = self.sender.send(HubMessage::Heartbeat { ts: timestamp_ms }) {
            debug!("dashboard websocket has no active listeners for heartbeat: {err}");
        }
    }
}

/// A single accumulated snapshot row paired with its canonical identity.
///
/// `row_signature` is the engine-stamped identity (`0` when unknown, in which
/// case matching falls back to an `id`/equality heuristic on `data`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SnapshotRow {
    #[serde(
        rename = "k",
        default,
        serialize_with = "serialize_row_signature",
        deserialize_with = "deserialize_row_signature"
    )]
    pub row_signature: u64,
    #[serde(rename = "v")]
    pub data: Value,
}

/// Snapshot of all query result rows, including grouped aggregates.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QuerySnapshot {
    pub rows: Vec<SnapshotRow>,
    // REST snapshots remain { rows }; only ordered WebSocket frames expose this boundary.
    #[serde(skip)]
    pub(super) revision: u64,
}

/// In-memory snapshot store keyed by query result identity.
/// Clients can fetch the current state to populate widgets on first load.
#[derive(Clone)]
pub struct QuerySnapshotStore {
    snapshots: Arc<RwLock<HashMap<String, QuerySnapshot>>>,
    max_rows_per_query: usize,
}

const DEFAULT_MAX_ROWS_PER_QUERY: usize = 10_000;

impl Default for QuerySnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

impl QuerySnapshotStore {
    pub fn new() -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            max_rows_per_query: DEFAULT_MAX_ROWS_PER_QUERY,
        }
    }

    pub fn with_max_rows(max_rows: usize) -> Self {
        Self {
            snapshots: Arc::new(RwLock::new(HashMap::new())),
            max_rows_per_query: max_rows,
        }
    }

    /// The configured per-query row cap.
    pub fn max_rows_per_query(&self) -> usize {
        self.max_rows_per_query
    }

    /// Update state and publish its revision under one lock, in the same order.
    pub async fn apply_and_broadcast(
        &self,
        query_result: &QueryResult,
        hub: &WebSocketHub,
    ) -> anyhow::Result<()> {
        let mut snapshots = self.snapshots.write().await;
        let snapshot = snapshots.entry(query_result.query_id.clone()).or_default();
        let revision = snapshot
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("dashboard query revision exhausted"))?;

        for diff in &query_result.results {
            match diff {
                ResultDiff::Add {
                    data,
                    row_signature,
                } => {
                    // Insert-of-an-existing-element is an upsert in drasi: replace
                    // an existing row with the same identity instead of appending a
                    // duplicate (see issue #605).
                    upsert_row(&mut snapshot.rows, *row_signature, data, None);
                }
                ResultDiff::Delete {
                    data,
                    row_signature,
                } => {
                    if let Some(idx) = find_row_index(&snapshot.rows, *row_signature, data) {
                        snapshot.rows.remove(idx);
                    }
                }
                ResultDiff::Update {
                    before,
                    after,
                    row_signature,
                    ..
                } => {
                    upsert_row(&mut snapshot.rows, *row_signature, after, Some(before));
                }
                ResultDiff::Aggregation {
                    before,
                    after,
                    row_signature,
                } => {
                    upsert_row(&mut snapshot.rows, *row_signature, after, before.as_ref());
                }
                ResultDiff::Noop => {}
            }
        }

        // Evict oldest rows if over limit (FIFO)
        if snapshot.rows.len() > self.max_rows_per_query {
            let excess = snapshot.rows.len() - self.max_rows_per_query;
            snapshot.rows.drain(..excess);
        }
        snapshot.revision = revision;
        hub.broadcast_query_result(query_result, revision);
        Ok(())
    }

    /// Seed the accumulated snapshot for a query with bootstrap rows.
    ///
    /// Used to populate the store from the initial bootstrap snapshot so the live
    /// WebSocket stream starts from the same baseline the fallback fetcher reads
    /// (see issue #605). Rows carry their `row_signature` so subsequent live diffs
    /// match them by identity. To avoid clobbering newer live data that may have
    /// already arrived, this only inserts rows whose identity is not already
    /// present; existing rows are left untouched. Respects `max_rows_per_query`.
    pub async fn seed_rows(&self, query_id: &str, rows: Vec<(u64, Value)>) {
        if rows.is_empty() {
            return;
        }
        let mut snapshots = self.snapshots.write().await;
        let snapshot = snapshots.entry(query_id.to_string()).or_default();

        for (row_signature, data) in rows {
            if find_row_index(&snapshot.rows, row_signature, &data).is_none() {
                snapshot.rows.push(SnapshotRow {
                    row_signature,
                    data,
                });
            }
        }

        // Evict oldest rows if over limit (FIFO)
        if snapshot.rows.len() > self.max_rows_per_query {
            let excess = snapshot.rows.len() - self.max_rows_per_query;
            snapshot.rows.drain(..excess);
        }
    }

    /// Get the current accumulated snapshot for a query.
    pub async fn get_snapshot(&self, query_id: &str) -> QuerySnapshot {
        let snapshots = self.snapshots.read().await;
        snapshots.get(query_id).cloned().unwrap_or_default()
    }

    /// Check whether there are any result rows for a query.
    pub async fn has_data(&self, query_id: &str) -> bool {
        let snapshots = self.snapshots.read().await;
        snapshots
            .get(query_id)
            .map(|s| !s.rows.is_empty())
            .unwrap_or(false)
    }
}

/// Insert or replace a row by its identity.
fn upsert_row(
    rows: &mut Vec<SnapshotRow>,
    row_signature: u64,
    data: &Value,
    before: Option<&Value>,
) {
    if let Some(idx) = find_row_index(rows, row_signature, before.unwrap_or(data)) {
        rows[idx] = SnapshotRow {
            row_signature,
            data: data.clone(),
        };
    } else {
        rows.push(SnapshotRow {
            row_signature,
            data: data.clone(),
        });
    }
}

/// Find a row by its canonical identity.
///
/// Matches on `row_signature` (the engine-stamped identity) when it is known
/// (non-zero); otherwise — or when the signature search misses (e.g. a row was
/// seeded with signature `0` via the FFI backward-compat fallback) — falls back
/// to matching by the `id` field, then by full data equality.
fn find_row_index(rows: &[SnapshotRow], row_signature: u64, data: &Value) -> Option<usize> {
    if row_signature != 0 {
        if let Some(idx) = rows.iter().position(|r| r.row_signature == row_signature) {
            return Some(idx);
        }
        // Fall through: a previously-seeded row may carry signature 0, so match
        // it by id/equality rather than appending a duplicate.
    }
    if let Some(target_id) = data.get("id") {
        rows.iter().position(|r| {
            (row_signature == 0 || r.row_signature == 0) && r.data.get("id") == Some(target_id)
        })
    } else {
        rows.iter()
            .position(|r| (row_signature == 0 || r.row_signature == 0) && &r.data == data)
    }
}

fn parse_client_message(raw: &str) -> Result<ClientMessage, serde_json::Error> {
    serde_json::from_str(raw)
}

fn serialize_server_message(message: ServerMessage) -> Result<String, serde_json::Error> {
    serde_json::to_string(&message)
}

/// Axum websocket upgrade handler.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    hub: WebSocketHub,
    snapshot_store: QuerySnapshotStore,
    query_ids: Vec<String>,
    shutdown_rx: watch::Receiver<bool>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, hub, snapshot_store, query_ids, shutdown_rx))
}

#[derive(Debug)]
struct QueryCursor {
    snapshot_revision: u64,
    delivered_revision: u64,
}

impl QueryCursor {
    fn after_snapshot(revision: u64) -> Self {
        Self {
            snapshot_revision: revision,
            delivered_revision: revision,
        }
    }

    fn accept(&mut self, revision: u64) -> anyhow::Result<bool> {
        if revision <= self.snapshot_revision {
            return Ok(false);
        }
        anyhow::ensure!(
            self.delivered_revision.checked_add(1) == Some(revision),
            "dashboard query revision gap or out-of-order delta"
        );
        self.delivered_revision = revision;
        Ok(true)
    }
}

async fn send_snapshots(
    sender: &mut SplitSink<WebSocket, Message>,
    store: &QuerySnapshotStore,
    query_ids: &[String],
    cursors: &mut HashMap<String, QueryCursor>,
) -> anyhow::Result<()> {
    for query_id in query_ids {
        let snapshot = store.get_snapshot(query_id).await;
        let message = ServerMessage::QuerySnapshot {
            query_id: query_id.clone(),
            revision: snapshot.revision,
            rows: snapshot.rows,
        };
        sender
            .send(Message::Text(serialize_server_message(message)?))
            .await?;
        cursors.insert(
            query_id.clone(),
            QueryCursor::after_snapshot(snapshot.revision),
        );
    }
    Ok(())
}

async fn handle_socket(
    socket: WebSocket,
    hub: WebSocketHub,
    snapshot_store: QuerySnapshotStore,
    query_ids: Vec<String>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let (mut sender, mut receiver) = socket.split();
    // Register before taking snapshots. Revisions exclude buffered deltas that
    // the snapshots already contain, without losing updates during transmission.
    let mut hub_receiver = hub.subscribe();
    let allowed_query_ids: HashSet<String> = query_ids.iter().cloned().collect();
    let session = async {
        let mut cursors = HashMap::new();
        // Keep the existing default subscription to all reaction queries.
        send_snapshots(&mut sender, &snapshot_store, &query_ids, &mut cursors).await?;
        loop {
            tokio::select! {
                message = hub_receiver.recv() => {
                    let hub_message = match message {
                        Ok(message) => message,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            anyhow::bail!("dashboard websocket lagged by {skipped}; reconnect for a fresh snapshot");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    if let HubMessage::QueryResult(payload) = &hub_message {
                        let Some(cursor) = cursors.get_mut(&payload.query_id) else {
                            continue;
                        };
                        if !cursor.accept(payload.revision)? {
                            continue;
                        }
                    }
                    let text = serialize_server_message(hub_message.into_server_message())?;
                    sender.send(Message::Text(text)).await?;
                }
                message = receiver.next() => {
                    let Some(message) = message else { break };
                    match message? {
                        Message::Text(text) => match parse_client_message(text.as_ref()) {
                            Ok(ClientMessage::Subscribe { query_ids }) => {
                                let authorized: Vec<_> = query_ids.into_iter()
                                    .filter(|id| allowed_query_ids.contains(id)).collect();
                                send_snapshots(&mut sender, &snapshot_store, &authorized, &mut cursors).await?;
                            }
                            Ok(ClientMessage::Unsubscribe { query_ids }) => {
                                for query_id in query_ids {
                                    cursors.remove(&query_id);
                                }
                            }
                            Err(err) => debug!("ignoring invalid websocket client message: {err}"),
                        },
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        biased;
        _ = shutdown_rx.wait_for(|signalled| *signalled) => {}
        result = session => {
            if let Err(err) = result {
                warn!("dashboard websocket closed; client must resynchronize: {err}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::collections::HashMap;

    #[test]
    fn test_parse_client_subscribe_message() {
        let parsed = parse_client_message(r#"{"type":"subscribe","query_ids":["q1","q2"]}"#)
            .expect("valid subscribe message");
        assert_eq!(
            parsed,
            ClientMessage::Subscribe {
                query_ids: vec!["q1".to_string(), "q2".to_string()]
            }
        );
    }

    #[test]
    fn test_parse_client_unsubscribe_message() {
        let parsed = parse_client_message(r#"{"type":"unsubscribe","query_ids":["q3"]}"#)
            .expect("valid unsubscribe message");
        assert_eq!(
            parsed,
            ClientMessage::Unsubscribe {
                query_ids: vec!["q3".to_string()]
            }
        );
    }

    #[test]
    fn test_query_result_message_serialization() {
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            sequence: 0,
            timestamp: Utc::now(),
            results: vec![
                ResultDiff::Add {
                    data: serde_json::json!({"name":"Alice"}),
                    row_signature: 0,
                },
                ResultDiff::Update {
                    data: serde_json::json!({"name":"Alice Updated"}),
                    before: serde_json::json!({"name":"Alice"}),
                    after: serde_json::json!({"name":"Alice Updated"}),
                    grouping_keys: None,
                    row_signature: 0,
                },
                ResultDiff::Delete {
                    data: serde_json::json!({"name":"Alice Updated"}),
                    row_signature: 0,
                },
            ],
            metadata: HashMap::new(),
            profiling: None,
        };

        let hub_message = HubMessage::from_query_result(&query_result, 12_345_678_901_234_567_890);
        let serialized = serialize_server_message(hub_message.into_server_message())
            .expect("server message should serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("serialized server message should be json");

        assert_eq!(parsed["type"], "query_result");
        assert_eq!(parsed["query_id"], "test-query");
        assert_eq!(parsed["revision"], "12345678901234567890");
        assert_eq!(parsed["results"][0]["op"], "add");
        assert_eq!(parsed["results"][1]["op"], "update");
        assert_eq!(parsed["results"][2]["op"], "delete");
        // row_signature is serialized as a string under `k` (64-bit precision-safe).
        assert_eq!(parsed["results"][0]["k"], "0");
    }

    #[test]
    fn test_row_signature_serialized_as_string_roundtrips() {
        let big = 12_345_678_901_234_567_890u64; // > 2^53, would lose precision as a JSON number
        let diff = WebSocketResultDiff {
            op: "add".to_string(),
            row_signature: big,
            data: Some(serde_json::json!({"id": 1})),
            before: None,
            after: None,
            grouping_keys: None,
        };
        let json = serde_json::to_value(&diff).unwrap();
        assert_eq!(json["k"], serde_json::Value::String(big.to_string()));
        let back: WebSocketResultDiff = serde_json::from_value(json).unwrap();
        assert_eq!(back.row_signature, big);
        // Also accepts a numeric form on the wire for resilience.
        let from_num: WebSocketResultDiff =
            serde_json::from_str(r#"{"op":"add","k":42,"data":{"id":1}}"#).unwrap();
        assert_eq!(from_num.row_signature, 42);
    }

    fn result(diff: ResultDiff) -> QueryResult {
        QueryResult {
            query_id: "q".into(),
            sequence: 0,
            timestamp: Utc::now(),
            results: vec![diff],
            metadata: HashMap::new(),
            profiling: None,
        }
    }

    fn add(value: i64) -> QueryResult {
        result(ResultDiff::Add {
            data: serde_json::json!({"value": value}),
            row_signature: 99,
        })
    }

    #[test]
    fn snapshot_cursor_excludes_overlap_but_rejects_live_gaps_and_reordering() {
        let mut cursor = QueryCursor::after_snapshot(10);
        assert!(!cursor.accept(9).unwrap());
        assert!(!cursor.accept(10).unwrap());
        assert!(cursor.accept(11).unwrap());
        assert!(cursor.accept(11).is_err());
        assert!(cursor.accept(13).is_err());
        assert!(cursor.accept(12).unwrap());
    }

    #[tokio::test]
    async fn snapshot_boundary_preserves_updates_and_empty_deletes() {
        let store = QuerySnapshotStore::new();
        let hub = WebSocketHub::new(8);
        let mut receiver = hub.subscribe();
        store.apply_and_broadcast(&add(1), &hub).await.unwrap();
        let snapshot = store.get_snapshot("q").await;
        let mut cursor = QueryCursor::after_snapshot(snapshot.revision);
        let HubMessage::QueryResult(overlap) = receiver.recv().await.unwrap() else {
            panic!("expected query result")
        };
        assert!(!cursor.accept(overlap.revision).unwrap());
        store.apply_and_broadcast(&add(2), &hub).await.unwrap();
        let HubMessage::QueryResult(live) = receiver.recv().await.unwrap() else {
            panic!("expected query result")
        };
        assert!(cursor.accept(live.revision).unwrap());
        store
            .apply_and_broadcast(
                &result(ResultDiff::Delete {
                    data: serde_json::json!({"value": 2}),
                    row_signature: 99,
                }),
                &hub,
            )
            .await
            .unwrap();
        let empty = store.get_snapshot("q").await;
        assert!(empty.rows.is_empty());
        assert_eq!(empty.revision, 3);
        let message = ServerMessage::QuerySnapshot {
            query_id: "q".into(),
            revision: empty.revision,
            rows: empty.rows,
        };
        let wire = serde_json::to_value(message).unwrap();
        assert_eq!(wire["rows"], serde_json::json!([]));
        assert_eq!(wire["revision"], "3");
    }

    #[tokio::test]
    async fn concurrent_updates_publish_in_revision_order() {
        let store = QuerySnapshotStore::new();
        let hub = WebSocketHub::new(8);
        let mut receiver = hub.subscribe();
        let first = add(1);
        let second = add(2);
        let (a, b) = tokio::join!(
            store.apply_and_broadcast(&first, &hub),
            store.apply_and_broadcast(&second, &hub),
        );
        a.unwrap();
        b.unwrap();
        let HubMessage::QueryResult(a) = receiver.recv().await.unwrap() else {
            panic!("expected first query result")
        };
        let HubMessage::QueryResult(b) = receiver.recv().await.unwrap() else {
            panic!("expected second query result")
        };
        assert_eq!((a.revision, b.revision), (1, 2));
        let snapshot = store.get_snapshot("q").await;
        assert_eq!(snapshot.revision, b.revision);
        assert_eq!(Some(&snapshot.rows[0].data), b.results[0].data.as_ref());
    }

    #[tokio::test]
    async fn revision_overflow_does_not_mutate_or_publish() {
        let store = QuerySnapshotStore::new();
        let hub = WebSocketHub::new(1);
        let mut receiver = hub.subscribe();
        store.snapshots.write().await.insert(
            "q".into(),
            QuerySnapshot {
                rows: vec![],
                revision: u64::MAX,
            },
        );
        assert!(store.apply_and_broadcast(&add(1), &hub).await.is_err());
        assert!(store.get_snapshot("q").await.rows.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn websocket_resubscription_replaces_snapshot_then_streams_live() {
        use axum::{routing::get, Router};
        use tokio_tungstenite::{connect_async, tungstenite};

        let store = QuerySnapshotStore::new();
        let hub = WebSocketHub::new(8);
        store.apply_and_broadcast(&add(1), &hub).await.unwrap();
        let (shutdown, mut shutdown_rx) = watch::channel(false);
        let socket_shutdown = shutdown.subscribe();
        let socket_store = store.clone();
        let socket_hub = hub.clone();
        let app = Router::new().route(
            "/ws",
            get(move |upgrade: WebSocketUpgrade| {
                ws_upgrade(
                    upgrade,
                    socket_hub.clone(),
                    socket_store.clone(),
                    vec!["q".into(), "empty".into()],
                    socket_shutdown.clone(),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.wait_for(|value| *value).await;
                })
                .await
                .unwrap();
        });
        let (mut socket, _) = connect_async(format!("ws://{address}/ws")).await.unwrap();
        async fn read(
            socket: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        ) -> Value {
            let message = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            serde_json::from_str(message.to_text().unwrap()).unwrap()
        }
        let snapshot = read(&mut socket).await;
        assert_eq!(snapshot["type"], "query_snapshot");
        assert_eq!(snapshot["revision"], "1");
        assert_eq!(snapshot["rows"][0]["v"]["value"], 1);
        let empty = read(&mut socket).await;
        assert_eq!(empty["query_id"], "empty");
        assert_eq!(empty["rows"], serde_json::json!([]));
        socket
            .send(tungstenite::Message::Text(
                r#"{"type":"subscribe","query_ids":["q"]}"#.into(),
            ))
            .await
            .unwrap();
        assert_eq!(read(&mut socket).await["revision"], "1");
        store.apply_and_broadcast(&add(2), &hub).await.unwrap();
        let delta = read(&mut socket).await;
        assert_eq!(delta["type"], "query_result");
        assert_eq!(delta["revision"], "2");
        assert_eq!(delta["results"][0]["data"]["value"], 2);
        // A gap cannot be silently accepted: the reader must reconnect.
        hub.broadcast_query_result(&add(3), 4);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .unwrap();
        assert!(matches!(
            closed,
            None | Some(Err(_)) | Some(Ok(tungstenite::Message::Close(_)))
        ));
        shutdown.send_replace(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
