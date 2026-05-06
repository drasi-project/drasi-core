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
use futures::{SinkExt, StreamExt};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

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
    pub timestamp: i64,
    pub results: Vec<WebSocketResultDiff>,
}

/// Server-to-client WebSocket messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    QueryResult {
        query_id: String,
        timestamp: i64,
        results: Vec<WebSocketResultDiff>,
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
                timestamp: payload.timestamp,
                results: payload.results,
            },
            HubMessage::Heartbeat { ts } => ServerMessage::Heartbeat { ts },
        }
    }

    /// Convert Drasi query results to websocket payload.
    pub fn from_query_result(query_result: &QueryResult) -> Self {
        let results = query_result
            .results
            .iter()
            .map(|result_diff| match result_diff {
                ResultDiff::Add { data, .. } => WebSocketResultDiff {
                    op: "add".to_string(),
                    data: Some(data.clone()),
                    before: None,
                    after: None,
                    grouping_keys: None,
                },
                ResultDiff::Delete { data, .. } => WebSocketResultDiff {
                    op: "delete".to_string(),
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
                    ..
                } => WebSocketResultDiff {
                    op: "update".to_string(),
                    data: Some(data.clone()),
                    before: Some(before.clone()),
                    after: Some(after.clone()),
                    grouping_keys: grouping_keys.clone(),
                },
                ResultDiff::Aggregation { before, after, .. } => WebSocketResultDiff {
                    op: "aggregation".to_string(),
                    data: None,
                    before: before.clone(),
                    after: Some(after.clone()),
                    grouping_keys: None,
                },
                ResultDiff::Noop => WebSocketResultDiff {
                    op: "noop".to_string(),
                    data: None,
                    before: None,
                    after: None,
                    grouping_keys: None,
                },
            })
            .collect();

        HubMessage::QueryResult(QueryResultPayload {
            query_id: query_result.query_id.clone(),
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

    pub fn broadcast_query_result(&self, query_result: &QueryResult) {
        if let Err(err) = self
            .sender
            .send(HubMessage::from_query_result(query_result))
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

/// Snapshot of accumulated query results, separating rows from aggregation state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QuerySnapshot {
    pub rows: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregation: Option<Value>,
}

/// In-memory snapshot store that accumulates query result rows and aggregation state.
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

    /// Apply a query result's diffs to the accumulated snapshot.
    pub async fn apply(&self, query_result: &QueryResult) {
        let mut snapshots = self.snapshots.write().await;
        let snapshot = snapshots.entry(query_result.query_id.clone()).or_default();

        for diff in &query_result.results {
            match diff {
                ResultDiff::Add { data, .. } => {
                    snapshot.rows.push(data.clone());
                }
                ResultDiff::Delete { data, .. } => {
                    if let Some(idx) = find_row_index(&snapshot.rows, data) {
                        snapshot.rows.remove(idx);
                    }
                }
                ResultDiff::Update { before, after, .. } => {
                    if let Some(idx) = find_row_index(&snapshot.rows, before) {
                        snapshot.rows[idx] = after.clone();
                    } else if let Some(idx) = find_row_index(&snapshot.rows, after) {
                        snapshot.rows[idx] = after.clone();
                    } else {
                        snapshot.rows.push(after.clone());
                    }
                }
                ResultDiff::Aggregation { after, .. } => {
                    snapshot.aggregation = Some(after.clone());
                }
                ResultDiff::Noop => {}
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

    /// Check whether there is any data (rows or aggregation) for a query.
    pub async fn has_data(&self, query_id: &str) -> bool {
        let snapshots = self.snapshots.read().await;
        snapshots
            .get(query_id)
            .map(|s| !s.rows.is_empty() || s.aggregation.is_some())
            .unwrap_or(false)
    }
}

fn find_row_index(rows: &[Value], target: &Value) -> Option<usize> {
    // Match by "id" field if present, otherwise by full equality.
    if let Some(target_id) = target.get("id") {
        rows.iter().position(|r| r.get("id") == Some(target_id))
    } else {
        rows.iter().position(|r| r == target)
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
    query_ids: Vec<String>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, hub, query_ids))
}

async fn handle_socket(socket: WebSocket, hub: WebSocketHub, query_ids: Vec<String>) {
    let (mut sender, mut receiver) = socket.split();
    let mut hub_receiver = hub.subscribe();

    // New clients are subscribed to all reaction queries by default.
    let subscriptions = Arc::new(RwLock::new(query_ids.into_iter().collect::<HashSet<_>>()));

    let subscriptions_for_send = Arc::clone(&subscriptions);
    let mut send_task = tokio::spawn(async move {
        loop {
            let hub_message = match hub_receiver.recv().await {
                Ok(message) => message,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!("dashboard websocket skipped {skipped} messages due to slow receiver");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            if let HubMessage::QueryResult(payload) = &hub_message {
                let is_subscribed = {
                    let subscriptions = subscriptions_for_send.read().await;
                    subscriptions.contains(&payload.query_id)
                };
                if !is_subscribed {
                    continue;
                }
            }

            let server_message = hub_message.into_server_message();
            let text = match serialize_server_message(server_message) {
                Ok(serialized) => serialized,
                Err(err) => {
                    error!("failed serializing websocket message: {err}");
                    continue;
                }
            };

            if sender.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    let subscriptions_for_receive = Arc::clone(&subscriptions);
    let mut receive_task = tokio::spawn(async move {
        while let Some(message_result) = receiver.next().await {
            let message = match message_result {
                Ok(message) => message,
                Err(err) => {
                    debug!("dashboard websocket receive error: {err}");
                    break;
                }
            };

            match message {
                Message::Text(text) => match parse_client_message(text.as_ref()) {
                    Ok(ClientMessage::Subscribe { query_ids }) => {
                        let mut subscriptions = subscriptions_for_receive.write().await;
                        for query_id in query_ids {
                            subscriptions.insert(query_id);
                        }
                    }
                    Ok(ClientMessage::Unsubscribe { query_ids }) => {
                        let mut subscriptions = subscriptions_for_receive.write().await;
                        for query_id in query_ids {
                            subscriptions.remove(&query_id);
                        }
                    }
                    Err(err) => {
                        debug!("ignoring invalid websocket client message: {err}");
                    }
                },
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut send_task => receive_task.abort(),
        _ = &mut receive_task => send_task.abort(),
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

        let hub_message = HubMessage::from_query_result(&query_result);
        let serialized = serialize_server_message(hub_message.into_server_message())
            .expect("server message should serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&serialized).expect("serialized server message should be json");

        assert_eq!(parsed["type"], "query_result");
        assert_eq!(parsed["query_id"], "test-query");
        assert_eq!(parsed["results"][0]["op"], "add");
        assert_eq!(parsed["results"][1]["op"], "update");
        assert_eq!(parsed["results"][2]["op"], "delete");
    }
}
