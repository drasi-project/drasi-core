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

//! In-process `ReactionService` gRPC mock server for the grpc reaction
//! integration tests.
//!
//! Unlike `src/test_server.rs` (which only records batch shape for unit
//! tests of the send/runner loops), this mock captures the **full**
//! `ProtoQueryResultItem` for every received call, so integration tests
//! can make wire-level assertions on `item_type`, `row_signature`,
//! `before`, `after`, and `sequence`.
//!
//! ASCII gRPC metadata headers on the inbound request are also captured
//! so tests can verify the D3 metadata-as-headers propagation.

#![allow(dead_code)]

use std::sync::Arc;

use drasi_reaction_grpc::proto::drasi_v1::reaction_service_server::{
    ReactionService, ReactionServiceServer,
};
use drasi_reaction_grpc::proto::drasi_v1::ProcessResultsResponse;
use drasi_reaction_grpc::ProcessResultsRequest;
use prost_types::Struct;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

/// One item recorded from the wire. Preserves the full v3 envelope so
/// tests can assert exact contents.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedItem {
    pub item_type: i32,
    pub row_signature: u64,
    pub before: Option<Struct>,
    pub after: Option<Struct>,
    pub sequence: u64,
    pub timestamp: Option<prost_types::Timestamp>,
    pub metadata: Option<Struct>,
    pub payload: Option<Struct>,
}

/// One `ProcessResults` invocation, with full per-item detail and the
/// ASCII metadata headers observed on the inbound request.
#[derive(Debug, Clone)]
pub struct RecordedBatch {
    pub query_id: String,
    pub items: Vec<RecordedItem>,
    pub metadata_headers: std::collections::HashMap<String, String>,
    pub had_rfc3339_timestamp: bool,
}

impl RecordedBatch {
    pub fn item_count(&self) -> usize {
        self.items.len()
    }
}

#[derive(Clone)]
pub struct Recorder {
    batches: Arc<Mutex<Vec<RecordedBatch>>>,
    notify: Arc<Notify>,
}

impl Recorder {
    fn new() -> Self {
        Self {
            batches: Arc::new(Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Snapshot of all recorded batches so far.
    pub async fn batches(&self) -> Vec<RecordedBatch> {
        self.batches.lock().await.clone()
    }

    /// Total number of items across all recorded batches.
    pub async fn total_items(&self) -> usize {
        self.batches
            .lock()
            .await
            .iter()
            .map(|b| b.items.len())
            .sum()
    }

    /// Total number of batches received.
    pub async fn batch_count(&self) -> usize {
        self.batches.lock().await.len()
    }

    /// Wait until at least `target` items have been recorded, or until
    /// `deadline` elapses. Returns the observed item count — never
    /// silently times out without returning the actual number, so
    /// callers can assert == target and surface flakes instead of
    /// hanging.
    pub async fn wait_for_items(&self, target: usize, deadline: std::time::Duration) -> usize {
        let start = std::time::Instant::now();
        loop {
            let total = self.total_items().await;
            if total >= target {
                return total;
            }
            let remaining = match deadline.checked_sub(start.elapsed()) {
                Some(r) if !r.is_zero() => r,
                _ => return self.total_items().await,
            };
            let _ = tokio::time::timeout(remaining, self.notify.notified()).await;
        }
    }
}

struct MockReactionService {
    recorder: Recorder,
}

#[tonic::async_trait]
impl ReactionService for MockReactionService {
    async fn process_results(
        &self,
        request: Request<ProcessResultsRequest>,
    ) -> Result<Response<ProcessResultsResponse>, Status> {
        // Snapshot ASCII headers (binary entries intentionally dropped).
        let metadata_headers: std::collections::HashMap<String, String> = request
            .metadata()
            .iter()
            .filter_map(|kv| match kv {
                tonic::metadata::KeyAndValueRef::Ascii(k, v) => {
                    Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))
                }
                tonic::metadata::KeyAndValueRef::Binary(_, _) => None,
            })
            .collect();

        let req = request.into_inner();
        let (query_id, items_in, had_rfc3339_timestamp) = match req.results {
            Some(r) => {
                // The sender writes a SystemTime → prost Timestamp. We
                // can sanity-check it as a real, post-epoch wall-clock
                // value (positive seconds).
                let rfc3339 = r.timestamp.as_ref().map(|t| t.seconds > 0).unwrap_or(false);
                (r.query_id, r.results, rfc3339)
            }
            None => (String::new(), Vec::new(), false),
        };

        let items: Vec<RecordedItem> = items_in
            .into_iter()
            .map(|it| RecordedItem {
                item_type: it.item_type,
                row_signature: it.row_signature,
                before: it.before,
                after: it.after,
                sequence: it.sequence,
                timestamp: it.timestamp,
                metadata: it.metadata,
                payload: it.payload,
            })
            .collect();
        let item_count = items.len();

        self.recorder.batches.lock().await.push(RecordedBatch {
            query_id,
            items,
            metadata_headers,
            had_rfc3339_timestamp,
        });
        self.recorder.notify.notify_waiters();

        Ok(Response::new(ProcessResultsResponse {
            success: true,
            message: "ok".to_string(),
            error: String::new(),
            items_processed: item_count as u32,
        }))
    }
}

/// Handle returned by [`start`]; dropping it (or calling
/// [`MockServer::shutdown`]) stops the background server task.
pub struct MockServer {
    pub endpoint: String,
    pub recorder: Recorder,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Signal the server to stop and wait for the task to finish.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

/// Spin up a mock `ReactionService` server bound to an ephemeral
/// loopback port.
pub async fn start() -> MockServer {
    let recorder = Recorder::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("read ephemeral local address");
    let endpoint = format!("grpc://{addr}");

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let service = MockReactionService {
        recorder: recorder.clone(),
    };

    let handle = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            .add_service(ReactionServiceServer::new(service))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    MockServer {
        endpoint,
        recorder,
        shutdown_tx: Some(shutdown_tx),
        handle,
    }
}

/// Decode a `prost_types::Struct` into `serde_json::Value` for ergonomic
/// assertions in tests.
pub fn struct_to_json(s: &Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (k, v) in &s.fields {
        map.insert(k.clone(), value_to_json(v));
    }
    serde_json::Value::Object(map)
}

fn value_to_json(v: &prost_types::Value) -> serde_json::Value {
    use prost_types::value::Kind;
    match &v.kind {
        Some(Kind::NullValue(_)) | None => serde_json::Value::Null,
        Some(Kind::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(Kind::NumberValue(n)) => {
            // The proto Struct wire format always carries numbers as f64.
            // For ergonomic test assertions, prefer the integer
            // representation when the value has no fractional part — so
            // `assert_eq!(..., json!({"id": 7}))` works against a value
            // that was emitted as `7.0` on the wire.
            if n.is_finite() && n.fract() == 0.0 && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                serde_json::Value::Number(serde_json::Number::from(*n as i64))
            } else {
                serde_json::Number::from_f64(*n)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null)
            }
        }
        Some(Kind::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(Kind::ListValue(l)) => {
            serde_json::Value::Array(l.values.iter().map(value_to_json).collect())
        }
        Some(Kind::StructValue(s)) => struct_to_json(s),
    }
}
