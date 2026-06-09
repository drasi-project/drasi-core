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

//! In-process mock `ReactionService` gRPC server used by the runner and
//! send-path integration tests.
//!
//! The server binds an ephemeral loopback port, records every
//! `ProcessResults` request it receives, and can be configured to fail the
//! first `N` requests (returning `success: false`) to exercise the retry /
//! reconnection paths in [`crate::send::send_batch_with_retry`].

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::{oneshot, Mutex, Notify};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use crate::proto::drasi_v1::reaction_service_server::{ReactionService, ReactionServiceServer};
use crate::proto::drasi_v1::{
    ProcessResultsRequest, ProcessResultsResponse, QueryResult as ProtoQueryResult,
    ReactionHealthCheckResponse, StreamResultsResponse, SubscribeRequest,
};

/// A single recorded `ProcessResults` invocation.
#[derive(Debug, Clone)]
pub(crate) struct RecordedBatch {
    pub query_id: String,
    pub item_count: usize,
    /// ASCII gRPC metadata headers observed on the inbound request.
    /// Binary headers and framework-internal entries (`te`, `user-agent`,
    /// `grpc-*`) are intentionally not filtered out — tests should assert
    /// presence of the keys they care about.
    pub metadata_headers: std::collections::HashMap<String, String>,
}

/// Shared, cloneable handle to the state captured by the mock server.
#[derive(Clone)]
pub(crate) struct Recorder {
    batches: Arc<Mutex<Vec<RecordedBatch>>>,
    /// Number of leading requests to reject with `success: false`.
    fail_remaining: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Recorder {
    fn new(fail_first: usize) -> Self {
        Self {
            batches: Arc::new(Mutex::new(Vec::new())),
            fail_remaining: Arc::new(AtomicUsize::new(fail_first)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Snapshot of all successfully recorded batches so far.
    pub(crate) async fn batches(&self) -> Vec<RecordedBatch> {
        self.batches.lock().await.clone()
    }

    /// Total number of items across all recorded batches.
    pub(crate) async fn total_items(&self) -> usize {
        self.batches.lock().await.iter().map(|b| b.item_count).sum()
    }

    /// Wait until at least `target` items have been recorded, or until
    /// `deadline` elapses. Returns the observed item count.
    pub(crate) async fn wait_for_items(
        &self,
        target: usize,
        deadline: std::time::Duration,
    ) -> usize {
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
        // Snapshot ASCII metadata headers before consuming the request.
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
        let (query_id, item_count) = req
            .results
            .map(|r| (r.query_id, r.results.len()))
            .unwrap_or_default();

        // Inject leading failures to drive retry logic.
        if self.recorder.fail_remaining.load(Ordering::SeqCst) > 0 {
            self.recorder.fail_remaining.fetch_sub(1, Ordering::SeqCst);
            return Ok(Response::new(ProcessResultsResponse {
                success: false,
                message: String::new(),
                error: "injected transient failure".to_string(),
                items_processed: 0,
            }));
        }

        self.recorder.batches.lock().await.push(RecordedBatch {
            query_id,
            item_count,
            metadata_headers,
        });
        self.recorder.notify.notify_waiters();

        Ok(Response::new(ProcessResultsResponse {
            success: true,
            message: "ok".to_string(),
            error: String::new(),
            items_processed: item_count as u32,
        }))
    }

    type StreamResultsStream = Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<StreamResultsResponse, Status>>
                + Send,
        >,
    >;

    async fn stream_results(
        &self,
        _request: Request<tonic::Streaming<ProtoQueryResult>>,
    ) -> Result<Response<Self::StreamResultsStream>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }

    type SubscribeStream = Pin<
        Box<
            dyn tonic::codegen::tokio_stream::Stream<Item = Result<ProtoQueryResult, Status>>
                + Send,
        >,
    >;

    async fn subscribe(
        &self,
        _request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }

    async fn health_check(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ReactionHealthCheckResponse>, Status> {
        Err(Status::unimplemented("not used in tests"))
    }
}

/// Handle returned by [`start`]; dropping it (or calling [`MockServer::shutdown`])
/// stops the background server task.
pub(crate) struct MockServer {
    pub endpoint: String,
    pub recorder: Recorder,
    shutdown_tx: Option<oneshot::Sender<()>>,
    handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// Signal the server to stop and wait for the task to finish.
    pub(crate) async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.handle.await;
    }
}

/// Start a mock server that accepts every request.
pub(crate) async fn start() -> MockServer {
    start_with_failures(0).await
}

/// Start a mock server that rejects the first `fail_first` requests with
/// `success: false` before accepting subsequent ones.
pub(crate) async fn start_with_failures(fail_first: usize) -> MockServer {
    let recorder = Recorder::new(fail_first);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
