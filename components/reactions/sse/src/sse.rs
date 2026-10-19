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

use anyhow::Context;
use async_trait::async_trait;
use axum::http::Method;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use drasi_reaction_common::sse::SseRenderer;
use log::{debug, error, info, warn};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use drasi_lib::channels::ComponentStatus;
use drasi_lib::context::workers::{
    cancel_owned_worker, join_owned_worker_gracefully, spawn_owned_worker, WorkerAlreadyOwned,
    WorkerCompletion,
};
use drasi_lib::observability::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{Reaction, SnapshotFetcher};

pub use super::config::SseReactionConfig;
use super::SseReactionBuilder;

const BROADCAST_CHANNEL_CAPACITY: usize = 1024;

fn subscriber_events(
    receiver: broadcast::Receiver<String>,
    path: String,
    mut shutdown: watch::Receiver<bool>,
) -> impl tokio_stream::Stream<
    Item = Result<Event, tokio_stream::wrappers::errors::BroadcastStreamRecvError>,
> {
    let events = tokio_stream::wrappers::BroadcastStream::new(receiver)
        .take_while(move |result| match result {
            Ok(_) => true,
            Err(error) => {
                warn!("Closing lagged SSE subscriber on {path}: {error}; reconnect and fetch a fresh snapshot");
                false
            }
        })
        .map(|result| result.map(|message| Event::default().data(message)));
    futures::StreamExt::take_until(events, async move {
        let _ = shutdown.wait_for(|stopped| *stopped).await;
    })
}

/// SSE reaction exposes query results to browser clients via Server-Sent Events.
pub struct SseReaction {
    pub(crate) base: ReactionBase,
    config: SseReactionConfig,
    broadcasters: Arc<tokio::sync::RwLock<HashMap<String, broadcast::Sender<String>>>>,
    lifecycle: Mutex<bool>,
    heartbeat_task: RwLock<Option<JoinHandle<()>>>,
    server_task: RwLock<Option<JoinHandle<std::io::Result<()>>>>,
    shutdown_tx: watch::Sender<bool>,
}

impl std::fmt::Debug for SseReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseReaction")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .field("broadcasters", &"<broadcasters>")
            .finish()
    }
}

impl SseReaction {
    /// Create a builder for SseReaction
    pub fn builder(id: impl Into<String>) -> SseReactionBuilder {
        SseReactionBuilder::new(id)
    }

    /// Create a new SSE reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: SseReactionConfig) -> Self {
        Self::create_internal(id.into(), queries, config, None, true)
    }

    /// Create a new SSE reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: SseReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        Self::create_internal(
            id.into(),
            queries,
            config,
            Some(priority_queue_capacity),
            true,
        )
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: SseReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    /// Internal constructor
    fn create_internal(
        id: String,
        queries: Vec<String>,
        config: SseReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        let renderer = SseRenderer::new(
            config.sse_path.clone(),
            config.routes.clone(),
            config.default_template.clone(),
        );
        let broadcasters = renderer
            .static_paths()
            .into_iter()
            .map(|path| (path, broadcast::channel(BROADCAST_CHANNEL_CAPACITY).0))
            .collect();

        Self {
            base: ReactionBase::new(params),
            config,
            broadcasters: Arc::new(tokio::sync::RwLock::new(broadcasters)),
            lifecycle: Mutex::new(false),
            heartbeat_task: RwLock::new(None),
            server_task: RwLock::new(None),
            shutdown_tx: watch::channel(false).0,
        }
    }
}

#[async_trait]
impl Reaction for SseReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "sse"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::SseReactionConfigDto;

        self.base
            .properties_or_serialize(&SseReactionConfigDto::from(&self.config))
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        let mut cleanup_required = self.lifecycle.lock().await;
        if *cleanup_required
            || self.server_task.read().await.is_some()
            || self.heartbeat_task.read().await.is_some()
            || self.base.processing_task.read().await.is_some()
            || self.shutdown_tx.receiver_count() != 0
        {
            return Err(anyhow::Error::new(WorkerAlreadyOwned).context(format!(
                "SSE reaction '{}' is already started; stop it before starting again",
                self.base.id
            )));
        }
        log_component_start("SSE Reaction", &self.base.id);

        *cleanup_required = true;
        // Transition to Starting
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting SSE reaction".to_string()),
            )
            .await;

        let host = self.config.host.clone();
        let port = self.config.port;
        let listener = match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpListener::bind((host.as_str(), port)),
        )
        .await
        .context("SSE listener bind timed out")
        .and_then(|result| result.map_err(anyhow::Error::from))
        .with_context(|| format!("Failed to bind SSE server on {host}:{port}"))
        {
            Ok(listener) => listener,
            Err(error) => {
                *cleanup_required = false;
                error!("[{}] {error:#}", self.base.id);
                self.base
                    .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                    .await;
                return Err(error);
            }
        };
        self.shutdown_tx.send_replace(false);

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Spawn processing task
        let status_handle = self.base.status_handle();
        let broadcasters = self.broadcasters.clone();
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();
        let renderer = SseRenderer::new(
            self.config.sse_path.clone(),
            self.config.routes.clone(),
            self.config.default_template.clone(),
        );
        spawn_owned_worker(&self.base.processing_task, async move {
            info!("[{reaction_id}] SSE result processing task started");

            loop {
                if !matches!(
                    status_handle.get_status().await,
                    ComponentStatus::Starting | ComponentStatus::Running
                ) {
                    info!("[{reaction_id}] SSE reaction not running, breaking loop");
                    break;
                }

                // Use select to wait for either a result OR shutdown signal
                let query_result = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                info!(
                    "[{}] Processing result from query '{}' with {} items",
                    reaction_id,
                    query_result.query_id,
                    query_result.results.len()
                );

                let timestamp = chrono::Utc::now().timestamp_millis();
                for event in renderer.render(&query_result, timestamp, |message| {
                    error!("[{reaction_id}] {message}");
                }) {
                    let broadcaster = {
                        let existing = broadcasters.read().await.get(&event.path).cloned();
                        match existing {
                            Some(sender) => sender,
                            None => broadcasters
                                .write()
                                .await
                                .entry(event.path.clone())
                                .or_insert_with(|| broadcast::channel(BROADCAST_CHANNEL_CAPACITY).0)
                                .clone(),
                        }
                    };
                    match broadcaster.send(event.payload) {
                        Ok(count) => debug!(
                            "[{reaction_id}] Broadcast to {count} SSE listeners on path {}",
                            event.path
                        ),
                        Err(error) => debug!(
                            "[{reaction_id}] no SSE listeners on path {}: {error}",
                            event.path
                        ),
                    }
                }
            }
            info!("[{reaction_id}] SSE result processing task ended");
        })
        .await?;

        // Heartbeat task - sends to all paths
        let broadcasters_hb = self.broadcasters.clone();
        let interval = self.config.heartbeat_interval_ms;
        spawn_owned_worker(&self.heartbeat_task, async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval));
            loop {
                ticker.tick().await;
                let beat = json!({"type":"heartbeat","ts": chrono::Utc::now().timestamp_millis()})
                    .to_string();
                // Send heartbeat to all broadcasters
                let broadcasters_read = broadcasters_hb.read().await;
                for broadcaster in broadcasters_read.values() {
                    let _ = broadcaster.send(beat.clone());
                }
            }
        })
        .await?;

        // HTTP server task - dynamically creates routes for all paths
        let broadcasters_server = self.broadcasters.clone();
        let mut server_shutdown = self.shutdown_tx.subscribe();
        let subscriber_shutdown = self.shutdown_tx.subscribe();
        let server_status = self.base.status_handle();

        // Get snapshot_fetcher from the runtime context for the /snapshot/:query_id endpoint
        let snapshot_fetcher = self
            .base
            .context()
            .await
            .and_then(|ctx| ctx.snapshot_fetcher.clone());

        spawn_owned_worker(&self.server_task, async move {
            // Configure CORS to allow all origins
            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::OPTIONS])
                .allow_headers(Any);

            // Create a handler that checks for matching paths dynamically
            let broadcasters_clone = broadcasters_server.clone();
            let handler = get(move |req: axum::http::Request<axum::body::Body>| {
                let broadcasters = broadcasters_clone.clone();
                let shutdown = subscriber_shutdown.clone();
                async move {
                    let path = req.uri().path().to_string();

                    // Try to find a broadcaster for this path
                    let broadcaster = {
                        let broadcasters_read = broadcasters.read().await;
                        broadcasters_read.get(&path).cloned()
                    };

                    if let Some(broadcaster) = broadcaster {
                        let rx = broadcaster.subscribe();
                        let stream = subscriber_events(rx, path, shutdown);
                        Sse::new(stream)
                            .keep_alive(
                                KeepAlive::new()
                                    .interval(Duration::from_secs(30))
                                    .text("keep-alive"),
                            )
                            .into_response()
                    } else {
                        // Return 404 for unknown paths
                        axum::http::StatusCode::NOT_FOUND.into_response()
                    }
                }
            });

            let app = Router::new()
                .route(
                    "/snapshot/:query_id",
                    get({
                        let sf = snapshot_fetcher.clone();
                        move |axum::extract::Path(query_id): axum::extract::Path<String>| {
                            let sf = sf.clone();
                            async move {
                                let fetcher = match sf.as_ref() {
                                    Some(f) => f,
                                    None => {
                                        return (
                                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                            "Snapshot fetcher not available",
                                        )
                                            .into_response();
                                    }
                                };
                                match fetcher.fetch_snapshot(&query_id).await {
                                    Ok(snapshot) => {
                                        // Stream the snapshot as a JSON array using chunked
                                        // transfer encoding. This keeps memory proportional to
                                        // a single row rather than the full result set.
                                        let is_first = std::sync::Arc::new(
                                            std::sync::atomic::AtomicBool::new(true),
                                        );
                                        let row_stream = snapshot.map(move |row| {
                                            let json = serde_json::to_string(&row)
                                                .unwrap_or_else(|_| "null".into());
                                            if is_first
                                                .swap(false, std::sync::atomic::Ordering::Relaxed)
                                            {
                                                json
                                            } else {
                                                format!(",{json}")
                                            }
                                        });

                                        let full_stream = tokio_stream::once("[".to_string())
                                            .chain(row_stream)
                                            .chain(tokio_stream::once("]".to_string()))
                                            .map(Ok::<_, std::convert::Infallible>);

                                        let body = axum::body::Body::from_stream(full_stream);
                                        match axum::response::Response::builder()
                                            .header("content-type", "application/json")
                                            .body(body)
                                        {
                                            Ok(resp) => resp.into_response(),
                                            Err(_) => (
                                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                                "Failed to build response",
                                            )
                                                .into_response(),
                                        }
                                    }
                                    Err(e) => {
                                        let msg = format!("Failed to fetch snapshot: {e}");
                                        (axum::http::StatusCode::NOT_FOUND, msg).into_response()
                                    }
                                }
                            }
                        }
                    }),
                )
                .fallback(handler)
                .layer(cors);

            info!("Starting SSE server on {host}:{port} with CORS enabled");
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = server_shutdown.wait_for(|stopped| *stopped).await;
                })
                .await;
            if let Err(error) = &result {
                error!("SSE server error: {error}");
                server_status
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("SSE server failed: {error}")),
                    )
                    .await;
            }
            result
        })
        .await?;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("SSE reaction started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let mut cleanup_required = self.lifecycle.lock().await;
        *cleanup_required = true;
        self.shutdown_tx.send_replace(true);
        let mut failures = Vec::new();
        if let Err(error) = cancel_owned_worker(
            &mut *self.heartbeat_task.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("SSE '{}' heartbeat cleanup", self.base.id))
        {
            failures.push(error);
        }
        match join_owned_worker_gracefully(
            &mut *self.server_task.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("SSE '{}' server cleanup", self.base.id))
        {
            Ok(WorkerCompletion::Completed(result)) => {
                if let Err(error) =
                    result.with_context(|| format!("SSE '{}' server failed", self.base.id))
                {
                    failures.push(error);
                }
            }
            Ok(WorkerCompletion::Cancelled) => failures.push(anyhow::anyhow!(
                "SSE '{}' server was unexpectedly cancelled",
                self.base.id
            )),
            Ok(WorkerCompletion::Absent) => {}
            Err(error) => failures.push(error),
        }
        if self.server_task.read().await.is_none() && self.heartbeat_task.read().await.is_none() {
            match tokio::time::timeout(Duration::from_secs(5), self.shutdown_tx.closed()).await {
                Ok(()) => {
                    if let Err(error) = self.base.stop_common().await {
                        failures.push(error);
                    }
                }
                Err(error) => failures.push(
                    anyhow::Error::new(error)
                        .context(format!("SSE '{}' connection cleanup", self.base.id)),
                ),
            }
        }
        if failures.is_empty() {
            *cleanup_required = false;
            return Ok(());
        }
        let error = if failures.len() == 1 {
            failures.remove(0)
        } else {
            anyhow::Error::new(drasi_lib::error::OperationFailures::new(
                format!("Failed to stop SSE reaction '{}'", self.base.id),
                failures,
            ))
        };
        error!("[{}] {error:#}", self.base.id);
        self.base
            .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
            .await;
        Err(error)
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        false
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> drasi_lib::recovery::ReactionRecoveryPolicy {
        drasi_lib::recovery::ReactionRecoveryPolicy::AutoSkipGap
    }
}

#[cfg(test)]
mod subscriber_stream_tests {
    use super::*;

    #[tokio::test]
    async fn lag_closes_the_subscriber_instead_of_sending_a_truncated_tail() {
        let (sender, receiver) = broadcast::channel(2);
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let stream = subscriber_events(receiver, "/events/test".into(), shutdown_rx);
        tokio::pin!(stream);
        sender.send("first".into()).unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        for event in ["second", "third", "fourth"] {
            sender.send(event.into()).unwrap();
        }
        assert!(
            stream.next().await.is_none(),
            "a lagged subscriber must reconnect and resnapshot"
        );
        let _ = sender.send("later".into());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn healthy_subscribers_receive_ordered_events_and_close_normally() {
        let (sender, receiver) = broadcast::channel(2);
        sender.send("first".into()).unwrap();
        sender.send("second".into()).unwrap();
        drop(sender);
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let response = Sse::new(subscriber_events(
            receiver,
            "/events/test".into(),
            shutdown_rx,
        ))
        .into_response();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "data: first\n\ndata: second\n\n"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_closes_idle_and_buffered_subscribers_before_more_events() {
        for buffered in [false, true] {
            let (sender, receiver) = broadcast::channel(2);
            let (shutdown, shutdown_rx) = watch::channel(false);
            let stream = subscriber_events(receiver, "/events/test".into(), shutdown_rx);
            tokio::pin!(stream);
            if buffered {
                sender.send("not delivered after shutdown".into()).unwrap();
            }
            shutdown.send_replace(true);
            assert!(stream.next().await.is_none());
            assert!(stream.next().await.is_none());
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use drasi_lib::context::workers::WorkerCleanupError;
    use tokio::sync::Notify;

    #[tokio::test(flavor = "current_thread")]
    async fn bind_failure_is_typed_and_never_starts_workers() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let reaction = SseReaction::builder("occupied")
            .with_host("127.0.0.1")
            .with_port(listener.local_addr()?.port())
            .build()?;
        let error = reaction.start().await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .expect("I/O cause")
                .kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction.heartbeat_task.read().await.is_none());
        assert!(reaction.server_task.read().await.is_none());
        drop(listener);
        reaction.start().await?;
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_cleanup_retains_worker_and_connection_owners() -> anyhow::Result<()> {
        for connection_only in [false, true] {
            let reaction = SseReaction::builder("cancelled").with_port(0).build()?;
            let release = Arc::new(Notify::new());
            let connection = if connection_only {
                Some(reaction.shutdown_tx.subscribe())
            } else {
                let drain = release.clone();
                spawn_owned_worker(&reaction.server_task, async move {
                    drain.notified().await;
                    Ok(())
                })
                .await?;
                None
            };
            {
                let stop = reaction.stop();
                tokio::pin!(stop);
                tokio::select! {
                    result = &mut stop => panic!("cleanup is still incomplete: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
            assert_eq!(
                reaction.server_task.read().await.is_some(),
                !connection_only
            );
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .downcast_ref::<WorkerAlreadyOwned>()
                .is_some());
            release.notify_one();
            drop(connection);
            reaction.stop().await?;
            reaction.start().await?;
            reaction.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_failures_preserve_io_and_panic_causes() -> anyhow::Result<()> {
        for panic in [false, true] {
            let reaction = SseReaction::builder("failed").with_port(0).build()?;
            spawn_owned_worker(&reaction.server_task, async move {
                assert!(!panic, "injected SSE server panic");
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected server failure",
                ))
            })
            .await?;
            let error = reaction.stop().await.unwrap_err();
            if panic {
                assert!(
                    matches!(error.downcast_ref(), Some(WorkerCleanupError::Join(error)) if error.is_panic())
                );
            } else {
                assert_eq!(
                    error
                        .downcast_ref::<std::io::Error>()
                        .expect("I/O cause")
                        .kind(),
                    std::io::ErrorKind::ConnectionReset
                );
            }
            assert!(reaction.server_task.read().await.is_none());
            reaction.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_timeout_keeps_the_actual_http_request_owned_until_it_finishes(
    ) -> anyhow::Result<()> {
        let reaction = SseReaction::builder("slow-request").with_port(0).build()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut shutdown = reaction.shutdown_tx.subscribe();
        let app = Router::new().route(
            "/",
            get({
                let entered = entered.clone();
                let release = release.clone();
                move || {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        "finished"
                    }
                }
            }),
        );
        spawn_owned_worker(&reaction.server_task, async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.wait_for(|stopped| *stopped).await;
                })
                .await
        })
        .await?;
        reaction
            .base
            .set_status(ComponentStatus::Running, None)
            .await;
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(12))
                .build()?
                .get(format!("http://{address}/"))
                .send()
                .await?
                .text()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
        let error = reaction.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(WorkerCleanupError::TimedOut { timeout }) if *timeout == Duration::from_secs(5))
        );
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert!(reaction.server_task.read().await.is_some());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        assert!(!request.is_finished());
        release.notify_one();
        assert_eq!(request.await??, "finished");
        reaction.stop().await?;
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        let rebound = tokio::net::TcpListener::bind(address).await?;
        drop(rebound);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_binding_still_requires_lifecycle_cleanup() -> anyhow::Result<()> {
        let (reaction, reservation) =
            crate::tests::sse_reaction_with_reserved_port("early-cancel").await;
        let address = reservation.local_addr()?;
        drop(reservation);
        let (updates, mut events) = tokio::sync::mpsc::channel(1);
        updates
            .send(drasi_lib::component_graph::ComponentUpdate::Status {
                component_id: reaction.id().into(),
                status: ComponentStatus::Stopped,
                message: None,
            })
            .await?;
        reaction.base.status_handle().wire(updates).await;
        let mut start = Box::pin(reaction.start());
        assert!(futures::poll!(&mut start).is_pending());
        assert_eq!(reaction.status().await, ComponentStatus::Starting);
        drop(start);
        assert!(*reaction.lifecycle.lock().await);
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        let rebound = tokio::net::TcpListener::bind(address).await?;
        drop(rebound);
        events.recv().await.unwrap();
        reaction.stop().await?;
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        assert!(!*reaction.lifecycle.lock().await);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn partial_start_requires_cleanup_even_without_spawned_workers() -> anyhow::Result<()> {
        let (reaction, reservation) =
            crate::tests::sse_reaction_with_reserved_port("partial").await;
        drop(reservation);
        let gate = reaction.base.shutdown_tx.write().await;
        let mut start = Box::pin(reaction.start());
        assert!(futures::poll!(&mut start).is_pending());
        drop(start);
        drop(gate);
        assert!(*reaction.lifecycle.lock().await);
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        reaction.stop().await?;
        reaction.start().await?;
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn owned_processor_delivers_results_while_starting() -> anyhow::Result<()> {
        use drasi_lib::channels::ResultDiff;
        use serde_json::Value;

        let (reaction, reservation) =
            crate::tests::sse_reaction_with_reserved_port("starting").await;
        drop(reservation);
        let mut results = reaction.broadcasters.read().await["/events"].subscribe();
        let shutdown_gate = reaction.base.shutdown_tx.write().await;
        let start = reaction.start();
        tokio::pin!(start);
        assert!(futures::poll!(&mut start).is_pending());
        let server_gate = reaction.server_task.write().await;
        drop(shutdown_gate);
        assert!(futures::poll!(&mut start).is_pending());
        assert_eq!(reaction.status().await, ComponentStatus::Starting);
        assert!(reaction.base.processing_task.read().await.is_some());
        reaction
            .enqueue_query_result(drasi_lib::channels::QueryResult::new(
                "starting-query".into(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: json!({"value": 42}),
                    row_signature: 0,
                }],
                Default::default(),
            ))
            .await?;
        let payload = tokio::time::timeout(Duration::from_secs(5), results.recv()).await??;
        assert_eq!(
            serde_json::from_str::<Value>(&payload)?["queryId"],
            "starting-query"
        );
        drop(server_gate);
        start.await?;
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn heartbeat_failure_still_cleans_processor_and_listener() -> anyhow::Result<()> {
        let (mut reaction, reservation) =
            crate::tests::sse_reaction_with_reserved_port("heartbeat-failure").await;
        let address = reservation.local_addr()?;
        drop(reservation);
        reaction.config.heartbeat_interval_ms = 0;
        let (updates, mut events) = tokio::sync::mpsc::channel(16);
        reaction.base.status_handle().wire(updates).await;
        reaction.start().await?;
        while !reaction
            .heartbeat_task
            .read()
            .await
            .as_ref()
            .unwrap()
            .is_finished()
        {
            tokio::task::yield_now().await;
        }
        let error = reaction.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(WorkerCleanupError::Join(error)) if error.is_panic())
        );
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction.server_task.read().await.is_none());
        assert!(reaction.heartbeat_task.read().await.is_none());
        let mut last = None;
        while let Ok(event) = events.try_recv() {
            last = Some(event);
        }
        assert!(
            matches!(last, Some(drasi_lib::component_graph::ComponentUpdate::Status {
            status: ComponentStatus::Error, message: Some(message), ..
        }) if message.contains("panicked"))
        );
        let rebound = tokio::net::TcpListener::bind(address).await?;
        drop(rebound);
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unabortable_heartbeat_retains_ownership_but_does_not_skip_server_cleanup(
    ) -> anyhow::Result<()> {
        let (reaction, reservation) =
            crate::tests::sse_reaction_with_reserved_port("blocked-heartbeat").await;
        let address = reservation.local_addr()?;
        drop(reservation);
        reaction.start().await?;
        cancel_owned_worker(
            &mut *reaction.heartbeat_task.write().await,
            Duration::from_secs(5),
        )
        .await?;
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        drasi_lib::context::workers::spawn_owned_blocking_worker(
            &reaction.heartbeat_task,
            move || {
                started.send(()).unwrap();
                wait.recv().unwrap();
            },
        )
        .await?;
        ready.await?;
        let result = tokio::time::timeout(Duration::from_secs(15), reaction.stop()).await;
        release.send(())?;
        let error = result?.unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(WorkerCleanupError::TimedOut { .. })
        ));
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert!(reaction.heartbeat_task.read().await.is_some());
        assert!(reaction.server_task.read().await.is_none());
        assert!(reaction.start().await.is_err());
        let rebound = tokio::net::TcpListener::bind(address).await?;
        drop(rebound);
        reaction.stop().await?;
        reaction.start().await?;
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restarts_close_live_streams_and_release_the_same_listener() -> anyhow::Result<()> {
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = reservation.local_addr()?.port();
        drop(reservation);
        let reaction = SseReaction::builder("restart")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_heartbeat_interval_ms(5)
            .build()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        reaction.stop().await?;
        for _ in 0..3 {
            reaction.start().await?;
            assert_eq!(
                client
                    .get(format!("http://127.0.0.1:{port}/snapshot/query"))
                    .send()
                    .await?
                    .status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );
            let response = client
                .get(format!("http://127.0.0.1:{port}/events"))
                .send()
                .await?;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let mut events = response.bytes_stream();
            let first = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await?
                .expect("live stream")?;
            assert!(std::str::from_utf8(&first)?.contains("heartbeat"));
            reaction.stop().await?;
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.heartbeat_task.read().await.is_none());
            assert!(reaction.server_task.read().await.is_none());
            assert_eq!(reaction.shutdown_tx.receiver_count(), 0);
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(event) = events.next().await {
                    event?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let rebound = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            drop(rebound);
        }
        reaction.stop().await?;
        Ok(())
    }
}
