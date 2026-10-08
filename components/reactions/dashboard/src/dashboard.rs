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

use anyhow::Context;
use async_trait::async_trait;
use axum::{
    extract::{ws::WebSocketUpgrade, Path},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use log::{debug, error, info};
use rust_embed::Embed;
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinHandle;
use tower_http::cors::{Any, CorsLayer};

use drasi_lib::channels::ComponentStatus;
use drasi_lib::context::workers::{
    cancel_owned_worker, join_owned_worker_gracefully, spawn_owned_worker, WorkerAlreadyOwned,
    WorkerCompletion,
};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use crate::api::{self, ApiState};
use crate::config::DashboardReactionConfig;
use crate::storage::{DashboardConfig, DashboardStorage};
use crate::websocket::{QuerySnapshotStore, WebSocketHub};
use crate::DashboardReactionBuilder;

const WEBSOCKET_BROADCAST_CAPACITY: usize = 1024;

#[derive(Embed)]
#[folder = "static/"]
struct DashboardAssets;

/// Dashboard reaction hosting a web UI and websocket data stream.
pub struct DashboardReaction {
    pub(crate) base: ReactionBase,
    config: DashboardReactionConfig,
    websocket_hub: WebSocketHub,
    snapshot_store: QuerySnapshotStore,
    lifecycle: tokio::sync::Mutex<()>,
    heartbeat_task: tokio::sync::RwLock<Option<JoinHandle<()>>>,
    /// Handle for the running axum HTTP/WebSocket server task.
    server_handle: tokio::sync::RwLock<Option<JoinHandle<std::io::Result<()>>>>,
    /// Signals the HTTP/WebSocket server to shut down gracefully. `true` means
    /// "shut down now": the axum server stops accepting and closes idle
    /// keep-alive connections, and WebSocket handlers terminate.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    predefined_dashboards: Vec<DashboardConfig>,
}

impl std::fmt::Debug for DashboardReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DashboardReaction")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .finish()
    }
}

impl DashboardReaction {
    /// Create a builder for DashboardReaction.
    pub fn builder(id: impl Into<String>) -> DashboardReactionBuilder {
        DashboardReactionBuilder::new(id)
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: DashboardReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
        predefined_dashboards: Vec<DashboardConfig>,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        Self {
            base: ReactionBase::new(params),
            config,
            websocket_hub: WebSocketHub::new(WEBSOCKET_BROADCAST_CAPACITY),
            snapshot_store: QuerySnapshotStore::new(),
            lifecycle: tokio::sync::Mutex::new(()),
            heartbeat_task: tokio::sync::RwLock::new(None),
            server_handle: tokio::sync::RwLock::new(None),
            shutdown_tx: tokio::sync::watch::channel(false).0,
            predefined_dashboards,
        }
    }
}

fn serve_asset(path: &str) -> Response {
    let clean_path = path.trim_start_matches('/');
    let effective_path = if clean_path.is_empty() {
        "index.html"
    } else {
        clean_path
    };

    match DashboardAssets::get(effective_path) {
        Some(content) => {
            let mime = content.metadata.mimetype();
            // Force revalidation so a browser tab can't serve a stale shell/JS
            // from its in-memory cache after the reaction is restarted.
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
                ],
                content.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn index_handler() -> Response {
    serve_asset("index.html")
}

async fn dashboard_page_handler(Path(_dashboard_id): Path<String>) -> Response {
    serve_asset("index.html")
}

async fn asset_handler(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

#[async_trait]
impl Reaction for DashboardReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "dashboard"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::DashboardReactionConfigDto;

        let dto = DashboardReactionConfigDto::from(&self.config);
        self.base.properties_or_serialize(&dto)
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
        let _lifecycle = self.lifecycle.lock().await;
        if self.server_handle.read().await.is_some()
            || self.heartbeat_task.read().await.is_some()
            || self.base.processing_task.read().await.is_some()
            || self.shutdown_tx.receiver_count() != 0
        {
            return Err(WorkerAlreadyOwned.into());
        }
        log_component_start("Dashboard Reaction", &self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting dashboard reaction".to_string()),
            )
            .await;

        let host = self.config.host.clone();
        let port = self.config.port;
        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(listener) => listener,
            Err(err) => {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!(
                            "Failed to bind dashboard server on {host}:{port}: {err}"
                        )),
                    )
                    .await;
                return Err(err)
                    .context(format!("failed to bind dashboard server on {host}:{port}"));
            }
        };

        // Seed predefined dashboards into the state store (skip if already exist).
        if !self.predefined_dashboards.is_empty() {
            if let Some(state_store) = self.base.state_store().await {
                let storage = DashboardStorage::new(self.base.id.clone(), state_store);
                for dashboard in &self.predefined_dashboards {
                    match storage.get_dashboard(&dashboard.id).await {
                        Ok(Some(_)) => {
                            debug!(
                                "predefined dashboard '{}' already exists, skipping",
                                dashboard.id
                            );
                        }
                        Ok(None) => match storage.save_dashboard(dashboard.clone()).await {
                            Ok(_) => {
                                info!(
                                    "seeded predefined dashboard '{}' ({})",
                                    dashboard.name, dashboard.id
                                );
                            }
                            Err(err) => {
                                error!(
                                    "failed to seed predefined dashboard '{}': {err}",
                                    dashboard.id
                                );
                            }
                        },
                        Err(err) => {
                            error!(
                                "failed to check predefined dashboard '{}': {err}",
                                dashboard.id
                            );
                        }
                    }
                }
            }
        }

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Dashboard reaction started".to_string()),
            )
            .await;

        let snapshot_fetcher = self
            .base
            .context()
            .await
            .and_then(|ctx| ctx.snapshot_fetcher.clone());

        // Seed the accumulated snapshot store from the bootstrap snapshot so the
        // live WebSocket stream starts from the same baseline the fallback fetcher
        // reads, instead of starting empty (see issue #605). This runs BEFORE the
        // processing task is spawned so a concurrently-arriving live diff (e.g. a
        // delete) can't be applied before seeding and then resurrected by the stale
        // bootstrap snapshot. Seeding is best-effort: failures (e.g. bootstrap not
        // yet complete) are logged and skipped, and the fallback fetcher in the API
        // still serves initial state. The collection is capped at the store's
        // per-query row limit to bound peak memory on large result sets.
        if let Some(fetcher) = snapshot_fetcher.as_ref() {
            let cap = self.snapshot_store.max_rows_per_query();
            for query_id in &self.base.queries {
                match fetcher.fetch_snapshot(query_id).await {
                    Ok(stream) => {
                        let rows = stream.collect_keyed_vec_capped(cap).await;
                        let count = rows.len();
                        self.snapshot_store.seed_rows(query_id, rows).await;
                        debug!(
                            "[{}] seeded {count} bootstrap row(s) for query '{query_id}'",
                            self.base.id
                        );
                    }
                    Err(err) => {
                        debug!(
                            "[{}] could not seed bootstrap snapshot for query '{query_id}': {err}",
                            self.base.id
                        );
                    }
                }
            }
        }

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let status_handle = self.base.status_handle();
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();
        let websocket_hub = self.websocket_hub.clone();
        let snapshot_store = self.snapshot_store.clone();
        spawn_owned_worker(&self.base.processing_task, async move {
            info!("[{reaction_id}] dashboard processing task started");

            loop {
                if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
                    info!("[{reaction_id}] dashboard reaction no longer running");
                    break;
                }

                let query_result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] dashboard processing shutdown signal");
                        break;
                    }
                    result = priority_queue.dequeue() => result,
                };

                snapshot_store.apply(query_result.as_ref()).await;
                websocket_hub.broadcast_query_result(query_result.as_ref());
            }

            info!("[{reaction_id}] dashboard processing task ended");
        })
        .await?;

        let heartbeat_interval_ms = self.config.heartbeat_interval_ms;
        let heartbeat_hub = self.websocket_hub.clone();
        let heartbeat_status_handle = self.base.status_handle();
        let heartbeat_reaction_id = self.base.id.clone();
        spawn_owned_worker(&self.heartbeat_task, async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
            loop {
                ticker.tick().await;
                if !matches!(
                    heartbeat_status_handle.get_status().await,
                    ComponentStatus::Running
                ) {
                    debug!("[{heartbeat_reaction_id}] heartbeat task stopped");
                    break;
                }
                heartbeat_hub.broadcast_heartbeat(chrono::Utc::now().timestamp_millis());
            }
        })
        .await?;

        let api_state = ApiState::new(
            self.base.id.clone(),
            self.base.queries.clone(),
            self.base.state_store().await,
            self.snapshot_store.clone(),
            snapshot_fetcher,
        );
        let websocket_hub = self.websocket_hub.clone();
        let query_ids = self.base.queries.clone();
        let server_status_handle = self.base.status_handle();
        // Reset the shutdown flag for this run (supports stop/start cycles) and
        // derive receivers for the server and per-connection WebSocket handlers.
        self.shutdown_tx.send_replace(false);
        let mut server_shutdown_rx = self.shutdown_tx.subscribe();
        let ws_shutdown_rx = self.shutdown_tx.subscribe();
        spawn_owned_worker(&self.server_handle, async move {
            let cors = CorsLayer::new()
                .allow_origin(tower_http::cors::AllowOrigin::exact(
                    format!("http://{host}:{port}")
                        .parse()
                        .unwrap_or_else(|_| axum::http::HeaderValue::from_static("null")),
                ))
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers(Any);

            let websocket_hub_for_route = websocket_hub.clone();
            let query_ids_for_route = query_ids.clone();
            let ws_shutdown_for_route = ws_shutdown_rx.clone();
            let app = Router::new()
                .route("/", get(index_handler))
                .route("/dashboard/:id", get(dashboard_page_handler))
                .route("/assets/*path", get(asset_handler))
                .route(
                    "/ws",
                    get(move |upgrade: WebSocketUpgrade| {
                        crate::websocket::ws_upgrade(
                            upgrade,
                            websocket_hub_for_route.clone(),
                            query_ids_for_route.clone(),
                            ws_shutdown_for_route.clone(),
                        )
                    }),
                )
                .merge(api::router().with_state(api_state))
                .layer(cors);

            let bound_addr = listener
                .local_addr()
                .map(|address| address.to_string())
                .unwrap_or_else(|_| format!("{host}:{port}"));
            info!("dashboard server listening on {bound_addr}");

            let server = axum::serve(listener, app).with_graceful_shutdown(async move {
                // Resolves when stop() sets the flag to `true`, or if the sender
                // is dropped (treated as shutdown).
                let _ = server_shutdown_rx.wait_for(|signalled| *signalled).await;
            });
            let result = server.await;
            if let Err(err) = &result {
                error!("dashboard server error: {err}");
                server_status_handle
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Dashboard server failed: {err}")),
                    )
                    .await;
            }
            result
        })
        .await?;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.shutdown_tx.send_replace(true);
        cancel_owned_worker(
            &mut *self.heartbeat_task.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("Dashboard '{}' heartbeat cleanup", self.base.id))?;
        if let WorkerCompletion::Completed(result) = join_owned_worker_gracefully(
            &mut *self.server_handle.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("Dashboard '{}' server cleanup", self.base.id))?
        {
            result.with_context(|| format!("Dashboard '{}' server failed", self.base.id))?;
        }
        tokio::time::timeout(Duration::from_secs(5), self.shutdown_tx.closed())
            .await
            .with_context(|| format!("Dashboard '{}' connection cleanup", self.base.id))?;
        self.base.stop_common().await
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

    async fn deprovision(&self) -> anyhow::Result<()> {
        self.base.deprovision_common().await
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use drasi_lib::context::workers::WorkerCleanupError;
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_dashboard_cleanup_retains_workers_and_blocks_restart() -> anyhow::Result<()>
    {
        let dashboard = DashboardReaction::builder("cancelled")
            .with_port(0)
            .build()?;
        let release = Arc::new(Notify::new());
        let drain = release.clone();
        spawn_owned_worker(&dashboard.server_handle, async move {
            drain.notified().await;
            Ok(())
        })
        .await?;
        {
            let stop = dashboard.stop();
            tokio::pin!(stop);
            tokio::select! {
                result = &mut stop => panic!("server must remain owned: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        assert!(dashboard.server_handle.read().await.is_some());
        assert!(dashboard
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        release.notify_one();
        dashboard.stop().await?;
        assert!(dashboard.server_handle.read().await.is_none());
        dashboard.start().await?;
        dashboard.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dashboard_join_preserves_server_errors_and_panics() -> anyhow::Result<()> {
        for panic in [false, true] {
            let dashboard = DashboardReaction::builder("failed").with_port(0).build()?;
            spawn_owned_worker(&dashboard.server_handle, async move {
                assert!(!panic, "injected dashboard server panic");
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected server failure",
                ))
            })
            .await?;
            let error = dashboard.stop().await.unwrap_err();
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
            assert!(dashboard.server_handle.read().await.is_none());
            dashboard.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_dashboard_connection_drain_blocks_restart_without_a_server_handle(
    ) -> anyhow::Result<()> {
        let dashboard = DashboardReaction::builder("connection")
            .with_port(0)
            .build()?;
        let connection = dashboard.shutdown_tx.subscribe();
        {
            let stop = dashboard.stop();
            tokio::pin!(stop);
            tokio::select! {
                result = &mut stop => panic!("connection must drain first: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
        assert!(dashboard.server_handle.read().await.is_none());
        assert!(dashboard
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        drop(connection);
        dashboard.stop().await?;
        dashboard.start().await?;
        dashboard.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dashboard_restarts_after_every_http_and_websocket_owner_exits() -> anyhow::Result<()> {
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = reservation.local_addr()?.port();
        drop(reservation);
        let dashboard = DashboardReaction::builder("restart")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_query("query")
            .with_heartbeat_interval_ms(10)
            .build()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        for _ in 0..3 {
            dashboard.start().await?;
            assert_eq!(
                client
                    .get(format!("http://127.0.0.1:{port}/"))
                    .send()
                    .await?
                    .status(),
                StatusCode::OK
            );
            let (mut socket, _) =
                tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/ws")).await?;
            let message = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await?
                .expect("active websocket")?;
            assert!(message.is_text());
            dashboard.stop().await?;
            assert!(dashboard.server_handle.read().await.is_none());
            assert!(dashboard.heartbeat_task.read().await.is_none());
            assert!(dashboard.base.processing_task.read().await.is_none());
            assert_eq!(dashboard.shutdown_tx.receiver_count(), 0);
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(message) = socket.next().await {
                    match message {
                        Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                        _ => {}
                    }
                }
            })
            .await?;
            let rebound = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            drop(rebound);
        }
        Ok(())
    }
}
