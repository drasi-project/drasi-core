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

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use axum::extract::State;
use axum::http::header::{HeaderName, AUTHORIZATION};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::{routing::post, Json, Router};
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use mcp_core::types::{
    Implementation, InitializeRequest, InitializeResponse, ReadResourceRequest,
    ReadResourceResponse, Resource, ResourceCapabilities, ResourceContents, ResourcesListResponse,
    ServerCapabilities, LATEST_PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, watch, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::{ReceiverStream, WatchStream};
use tokio_stream::Stream;
use tower_http::cors::{Any, CorsLayer};
use url::Url;
use uuid::Uuid;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::context::workers::{
    join_owned_worker_gracefully, spawn_owned_worker, WorkerAlreadyOwned, WorkerCompletion,
};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::Reaction;

use super::config::{McpReactionConfig, NotificationTemplate, QueryConfig};
use super::{register_json_helper, McpReactionBuilder};

const DEFAULT_ADDED_TEMPLATE: &str = r#"{"operation":"added","data":{{json after}}}"#;
const DEFAULT_UPDATED_TEMPLATE: &str =
    r#"{"operation":"updated","before":{{json before}},"after":{{json after}}}"#;
const DEFAULT_DELETED_TEMPLATE: &str = r#"{"operation":"deleted","data":{{json before}}}"#;

const MCP_SESSION_HEADER: &str = "mcp-session-id";

#[derive(Debug, Clone)]
struct SessionState {
    sender: mpsc::Sender<String>,
    receiver: Arc<Mutex<Option<mpsc::Receiver<String>>>>,
}

#[derive(Debug)]
struct McpState {
    reaction_id: String,
    config: McpReactionConfig,
    query_ids: Vec<String>,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    subscriptions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    subscribers_by_uri: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    current_results: Arc<RwLock<HashMap<String, Vec<Value>>>>,
    session_cleanup: Arc<Notify>,
}

impl McpState {
    async fn add_subscription(&self, session_id: &str, uri: &str) -> bool {
        let sessions = self.sessions.read().await;
        let active = sessions
            .get(session_id)
            .is_some_and(|session| !session.sender.is_closed());
        if !active {
            warn!(
                "[{}] Subscription rejected for closed session {session_id}",
                self.reaction_id
            );
            return false;
        }
        let mut subscriptions = self.subscriptions.write().await;
        subscriptions
            .entry(session_id.to_string())
            .or_default()
            .insert(uri.to_string());

        let mut subscribers_by_uri = self.subscribers_by_uri.write().await;
        subscribers_by_uri
            .entry(uri.to_string())
            .or_default()
            .insert(session_id.to_string());

        let subscriber_count = subscribers_by_uri.get(uri).map_or(0, |s| s.len());
        debug!(
            "[{}] Session {session_id} subscribed to {uri} ({subscriber_count} total subscribers)",
            self.reaction_id
        );
        drop(sessions);
        true
    }

    async fn remove_subscription(&self, session_id: &str, uri: &str) {
        let mut subscriptions = self.subscriptions.write().await;
        if let Some(set) = subscriptions.get_mut(session_id) {
            set.remove(uri);
            if set.is_empty() {
                subscriptions.remove(session_id);
            }
        }

        let mut subscribers_by_uri = self.subscribers_by_uri.write().await;
        if let Some(set) = subscribers_by_uri.get_mut(uri) {
            set.remove(session_id);
            if set.is_empty() {
                subscribers_by_uri.remove(uri);
            }
        }

        debug!(
            "[{}] Session {session_id} unsubscribed from {uri}",
            self.reaction_id
        );
    }

    async fn cleanup_session(&self, session_id: &str) {
        self.sessions.write().await.remove(session_id);

        let mut subscriptions = self.subscriptions.write().await;
        if let Some(uris) = subscriptions.remove(session_id) {
            debug!(
                "[{}] Cleaning up session {session_id} ({} subscriptions)",
                self.reaction_id,
                uris.len()
            );
            let mut subscribers_by_uri = self.subscribers_by_uri.write().await;
            for uri in uris {
                if let Some(set) = subscribers_by_uri.get_mut(&uri) {
                    set.remove(session_id);
                    if set.is_empty() {
                        subscribers_by_uri.remove(&uri);
                    }
                }
            }
        } else {
            debug!(
                "[{}] Cleaning up session {session_id} (no subscriptions)",
                self.reaction_id
            );
        }
    }

    async fn cleanup_closed_sessions(&self) {
        let closed: Vec<_> = self
            .sessions
            .read()
            .await
            .iter()
            .filter(|(_, session)| session.sender.is_closed())
            .map(|(id, _)| id.clone())
            .collect();
        for session_id in closed {
            self.cleanup_session(&session_id).await;
        }
    }
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcErrorObject>,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

struct SessionStream {
    inner: ReceiverStream<String>,
    shutdown: WatchStream<bool>,
    stopped: bool,
    state: Arc<McpState>,
    session_id: String,
}

impl Stream for SessionStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.stopped {
            return Poll::Ready(None);
        }
        while let Poll::Ready(shutdown) = Pin::new(&mut this.shutdown).poll_next(cx) {
            if shutdown != Some(false) {
                this.stopped = true;
                this.inner.close();
                return Poll::Ready(None);
            }
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(message)) => Poll::Ready(Some(Ok(Event::default().data(message)))),
            Poll::Ready(None) => {
                this.stopped = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        self.inner.close();
        info!(
            "[{}] SSE stream closed for session {}",
            self.state.reaction_id, self.session_id
        );
        self.state.session_cleanup.notify_one();
    }
}

/// MCP reaction exposes Drasi query results via MCP protocol.
pub struct McpReaction {
    pub(crate) base: ReactionBase,
    config: McpReactionConfig,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    subscriptions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    subscribers_by_uri: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    current_results: Arc<RwLock<HashMap<String, Vec<Value>>>>,
    cleanup_required: Mutex<bool>,
    server_task: RwLock<Option<JoinHandle<std::io::Result<()>>>>,
    shutdown_tx: watch::Sender<bool>,
    session_cleanup: Arc<Notify>,
    bound_port: Arc<AtomicU16>,
}

impl std::fmt::Debug for McpReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpReaction")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .field("sessions", &"<sessions>")
            .finish()
    }
}

impl McpReaction {
    /// Create a builder for McpReaction.
    pub fn builder(id: impl Into<String>) -> McpReactionBuilder {
        McpReactionBuilder::new(id)
    }

    /// Create a new MCP reaction.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: McpReactionConfig) -> Self {
        Self::create_internal(id.into(), queries, config, None, true)
    }

    /// Create a new MCP reaction with custom priority queue capacity.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: McpReactionConfig,
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

    /// Create from builder (internal method).
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: McpReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    fn create_internal(
        id: String,
        queries: Vec<String>,
        config: McpReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        Self {
            base: ReactionBase::new(params),
            config,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            subscribers_by_uri: Arc::new(RwLock::new(HashMap::new())),
            current_results: Arc::new(RwLock::new(HashMap::new())),
            cleanup_required: Mutex::new(false),
            server_task: RwLock::new(None),
            shutdown_tx: watch::channel(false).0,
            session_cleanup: Arc::new(Notify::new()),
            bound_port: Arc::new(AtomicU16::new(0)),
        }
    }

    pub(crate) fn config(&self) -> &McpReactionConfig {
        &self.config
    }

    /// Get the actual bound port (useful when configured with port 0).
    pub fn bound_port(&self) -> u16 {
        let bound = self.bound_port.load(Ordering::SeqCst);
        if bound == 0 {
            self.config.port
        } else {
            bound
        }
    }

    /// Get a handle to the bound port for external observation.
    pub fn bound_port_handle(&self) -> Arc<AtomicU16> {
        self.bound_port.clone()
    }
}

#[async_trait]
impl Reaction for McpReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mcp"
    }

    fn properties(&self) -> HashMap<String, Value> {
        use crate::descriptor::McpReactionConfigDto;

        self.base
            .properties_or_serialize(&McpReactionConfigDto::from(&self.config))
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

    async fn start(&self) -> Result<()> {
        let mut cleanup_required = self.cleanup_required.lock().await;
        if *cleanup_required
            || self.server_task.read().await.is_some()
            || self.base.processing_task.read().await.is_some()
            || self.shutdown_tx.receiver_count() != 0
        {
            return Err(WorkerAlreadyOwned.into());
        }
        log_component_start("MCP Reaction", &self.base.id);

        info!(
            "[{}] Starting MCP server on {}:{} (auth={}, max_sessions={}, queries={:?})",
            self.base.id,
            self.config.host,
            self.config.port,
            self.config.bearer_token.is_some(),
            self.config.max_sessions,
            self.base.queries
        );

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting MCP reaction".to_string()),
            )
            .await;
        self.bound_port.store(0, Ordering::SeqCst);
        let listener = match tokio::net::TcpListener::bind((
            self.config.host.as_str(),
            self.config.port,
        ))
        .await
        {
            Ok(listener) => listener,
            Err(error) => {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to bind MCP server: {error}")),
                    )
                    .await;
                return Err(error).context("failed to bind MCP server");
            }
        };
        let address = listener
            .local_addr()
            .context("failed to inspect MCP listener")?;
        *cleanup_required = true;
        self.shutdown_tx.send_replace(false);
        self.bound_port.store(address.port(), Ordering::SeqCst);
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("MCP reaction started".to_string()),
            )
            .await;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let state = Arc::new(McpState {
            reaction_id: self.base.id.clone(),
            config: self.config.clone(),
            query_ids: self.base.queries.clone(),
            sessions: self.sessions.clone(),
            subscriptions: self.subscriptions.clone(),
            subscribers_by_uri: self.subscribers_by_uri.clone(),
            current_results: self.current_results.clone(),
            session_cleanup: self.session_cleanup.clone(),
        });

        let server_state = state.clone();
        let server_shutdown = self.shutdown_tx.subscribe();
        let server_status = self.base.status_handle();
        spawn_owned_worker(&self.server_task, async move {
            let result = run_server(server_state, listener, server_shutdown).await;
            if let Err(error) = &result {
                error!("MCP server failed: {error}");
                server_status
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("MCP server failed: {error}")),
                    )
                    .await;
            }
            result
        })
        .await?;

        let status_handle = self.base.status_handle();
        let priority_queue = self.base.priority_queue.clone();
        let query_configs = self.config.routes.clone();
        let processing_state = state.clone();

        spawn_owned_worker(&self.base.processing_task, async move {
            let reaction_id = &processing_state.reaction_id;
            info!("[{reaction_id}] MCP result processing task started");
            let mut handlebars = Handlebars::new();
            register_json_helper(&mut handlebars);

            loop {
                if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
                    break;
                }

                let query_result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                        break;
                    }
                    _ = processing_state.session_cleanup.notified() => {
                        processing_state.cleanup_closed_sessions().await;
                        continue;
                    }
                    result = priority_queue.dequeue() => result,
                };

                if query_result.results.is_empty() {
                    debug!("[{reaction_id}] Received empty result set from query, skipping");
                    continue;
                }

                let query_id = query_result.query_id.clone();
                let query_config = get_query_config(&query_id, &query_configs);
                let uri = format!("drasi://query/{query_id}");

                debug!(
                    "[{reaction_id}] Processing {} result diffs for query '{query_id}'",
                    query_result.results.len()
                );

                for diff in &query_result.results {
                    apply_diff(&query_id, diff, &processing_state.current_results).await;

                    let (template, operation) = match diff {
                        ResultDiff::Add { .. } => {
                            (template_for(query_config, DiffKind::Add), "added")
                        }
                        ResultDiff::Update { .. } => {
                            (template_for(query_config, DiffKind::Update), "updated")
                        }
                        ResultDiff::Delete { .. } => {
                            (template_for(query_config, DiffKind::Delete), "deleted")
                        }
                        ResultDiff::Aggregation { .. } => {
                            (template_for(query_config, DiffKind::Update), "updated")
                        }
                        ResultDiff::Noop => (None, "noop"),
                    };

                    let template = match template {
                        Some(t) => t,
                        None => continue,
                    };

                    let mut context = Map::new();
                    context.insert("queryId".to_string(), Value::String(query_id.clone()));

                    match diff {
                        ResultDiff::Add { data, .. } => {
                            context.insert("after".to_string(), data.clone());
                        }
                        ResultDiff::Update {
                            data,
                            before,
                            after,
                            ..
                        } => {
                            context.insert("before".to_string(), before.clone());
                            context.insert("after".to_string(), after.clone());
                            context.insert("data".to_string(), data.clone());
                        }
                        ResultDiff::Delete { data, .. } => {
                            context.insert("before".to_string(), data.clone());
                        }
                        ResultDiff::Aggregation { before, after, .. } => {
                            if let Some(before) = before {
                                context.insert("before".to_string(), before.clone());
                            }
                            context.insert("after".to_string(), after.clone());
                        }
                        ResultDiff::Noop => {}
                    }

                    let rendered = match handlebars.render_template(&template.template, &context) {
                        Ok(rendered) => rendered,
                        Err(err) => {
                            warn!(
                                "[{reaction_id}] Failed to render template for {query_id}: {err}"
                            );
                            continue;
                        }
                    };

                    debug!(
                        "[{reaction_id}] Rendered template for {query_id}/{operation}: {rendered}"
                    );

                    let payload: Value = match serde_json::from_str(&rendered) {
                        Ok(value) => value,
                        Err(err) => {
                            warn!(
                                "[{reaction_id}] Template output was not valid JSON for {query_id}: {err}. \
                                 Rendered output: '{rendered}'"
                            );
                            continue;
                        }
                    };

                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/resources/updated",
                        "params": {
                            "uri": uri,
                            "operation": operation,
                            "data": payload
                        }
                    });

                    let notification_text = match serde_json::to_string(&notification) {
                        Ok(text) => text,
                        Err(err) => {
                            warn!("[{reaction_id}] Failed to serialize notification: {err}");
                            continue;
                        }
                    };

                    let subscribed_sessions = {
                        let subscribers = processing_state.subscribers_by_uri.read().await;
                        subscribers.get(&uri).cloned().unwrap_or_else(HashSet::new)
                    };

                    if subscribed_sessions.is_empty() {
                        debug!(
                            "[{reaction_id}] No subscribers for {uri}, skipping {operation} notification"
                        );
                        continue;
                    }

                    debug!(
                        "[{reaction_id}] Sending {operation} notification for {uri} to {} session(s)",
                        subscribed_sessions.len()
                    );

                    let mut to_cleanup = Vec::new();
                    {
                        let sessions_guard = processing_state.sessions.read().await;
                        for session_id in subscribed_sessions {
                            if let Some(session) = sessions_guard.get(&session_id) {
                                match session.sender.try_send(notification_text.clone()) {
                                    Ok(_) => {}
                                    Err(mpsc::error::TrySendError::Full(_)) => {
                                        warn!("[{reaction_id}] Session {session_id} channel full, disconnecting");
                                        to_cleanup.push(session_id);
                                    }
                                    Err(mpsc::error::TrySendError::Closed(_)) => {
                                        to_cleanup.push(session_id);
                                    }
                                }
                            }
                        }
                    }

                    for session_id in to_cleanup {
                        processing_state.cleanup_session(&session_id).await;
                    }
                }
            }
        }).await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut cleanup_required = self.cleanup_required.lock().await;
        self.shutdown_tx.send_replace(true);
        info!("[{}] Stopping MCP reaction", self.base.id);
        let session_count = self.sessions.read().await.len();
        if session_count > 0 {
            info!(
                "[{}] Disconnecting {session_count} active session(s)",
                self.base.id
            );
        }

        if let WorkerCompletion::Completed(result) = join_owned_worker_gracefully(
            &mut *self.server_task.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("MCP '{}' server cleanup", self.base.id))?
        {
            result.with_context(|| format!("MCP '{}' server failed", self.base.id))?;
        }
        tokio::time::timeout(Duration::from_secs(5), self.shutdown_tx.closed())
            .await
            .with_context(|| format!("MCP '{}' connection cleanup", self.base.id))?;
        self.sessions.write().await.clear();
        self.subscriptions.write().await.clear();
        self.subscribers_by_uri.write().await.clear();
        self.base.stop_common().await?;
        self.bound_port.store(0, Ordering::SeqCst);
        *cleanup_required = false;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        debug!(
            "[{}] Enqueuing {} result diff(s) for query '{}'",
            self.base.id,
            result.results.len(),
            result.query_id
        );
        self.base.enqueue_query_result(result).await
    }
}

#[derive(Clone, Copy)]
enum DiffKind {
    Add,
    Update,
    Delete,
}

fn template_for(config: Option<&QueryConfig>, kind: DiffKind) -> Option<NotificationTemplate> {
    match kind {
        DiffKind::Add => config.and_then(|cfg| cfg.added.clone()).or_else(|| {
            Some(NotificationTemplate {
                template: DEFAULT_ADDED_TEMPLATE.to_string(),
            })
        }),
        DiffKind::Update => config.and_then(|cfg| cfg.updated.clone()).or_else(|| {
            Some(NotificationTemplate {
                template: DEFAULT_UPDATED_TEMPLATE.to_string(),
            })
        }),
        DiffKind::Delete => config.and_then(|cfg| cfg.deleted.clone()).or_else(|| {
            Some(NotificationTemplate {
                template: DEFAULT_DELETED_TEMPLATE.to_string(),
            })
        }),
    }
}

fn get_query_config<'a>(
    query_id: &str,
    routes: &'a HashMap<String, QueryConfig>,
) -> Option<&'a QueryConfig> {
    routes.get(query_id).or_else(|| {
        if query_id.contains('.') {
            query_id
                .rsplit('.')
                .next()
                .and_then(|name| routes.get(name))
        } else {
            None
        }
    })
}

async fn apply_diff(
    query_id: &str,
    diff: &ResultDiff,
    current: &Arc<RwLock<HashMap<String, Vec<Value>>>>,
) {
    let mut results = current.write().await;
    let entry = results.entry(query_id.to_string()).or_default();
    match diff {
        ResultDiff::Add { data, .. } => entry.push(data.clone()),
        ResultDiff::Delete { data, .. } => remove_first(entry, data),
        ResultDiff::Update { before, after, .. } => {
            remove_first(entry, before);
            entry.push(after.clone());
        }
        ResultDiff::Aggregation { before, after, .. } => {
            if let Some(before) = before {
                remove_first(entry, before);
            }
            entry.push(after.clone());
        }
        ResultDiff::Noop => {}
    }
}

/// Remove the first occurrence of `target` from the list.
/// NOTE: This is O(n) per call. For queries with very large result sets, consider
/// keying results by a stable identifier (e.g., primary key) for O(1) removal.
fn remove_first(list: &mut Vec<Value>, target: &Value) {
    if let Some(index) = list.iter().position(|item| item == target) {
        list.remove(index);
    }
}

const QUERY_URI_PREFIX: &str = "drasi://query/";

/// Validate a subscription URI and extract the query ID.
/// Returns `Some(query_id)` if the URI is well-formed and matches a known query.
fn validate_subscription_uri<'a>(uri: &'a str, query_ids: &[String]) -> Option<&'a str> {
    let query_id = uri.strip_prefix(QUERY_URI_PREFIX)?;
    if query_id.is_empty() {
        return None;
    }
    if query_ids.iter().any(|id| id == query_id) {
        Some(query_id)
    } else {
        None
    }
}

fn resource_uri(query_id: &str) -> Option<Url> {
    Url::parse(&format!("drasi://query/{query_id}")).ok()
}

/// Maximum length for a session ID header value.
const MAX_SESSION_ID_LEN: usize = 64;

fn parse_bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .map(str::to_string)
}

fn check_authorization(headers: &HeaderMap, token: &Option<String>) -> bool {
    let expected = match token {
        Some(token) => token,
        None => return true,
    };
    match parse_bearer_token(headers) {
        Some(provided) => expected.as_bytes().ct_eq(provided.as_bytes()).into(),
        None => false,
    }
}

fn jsonrpc_error(
    id: Option<Value>,
    code: i64,
    message: impl Into<String>,
) -> Json<JsonRpcResponse> {
    Json(JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcErrorObject {
            code,
            message: message.into(),
        }),
    })
}

fn jsonrpc_result(id: Option<Value>, result: Value) -> Json<JsonRpcResponse> {
    Json(make_result(id, result))
}

fn make_result(id: Option<Value>, result: Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

fn make_error(id: Option<Value>, code: i64, message: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(JsonRpcErrorObject {
            code,
            message: message.into(),
        }),
    }
}

type HttpState = (Arc<McpState>, watch::Receiver<bool>);

async fn run_server(
    state: Arc<McpState>,
    listener: tokio::net::TcpListener,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", post(handle_post).get(handle_sse))
        .with_state((state.clone(), shutdown.clone()))
        .layer(
            CorsLayer::new()
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers(Any)
                // TODO: make allowed origins configurable for production deployments
                .allow_origin(Any),
        );

    let addr = listener.local_addr()?;

    info!("[{}] MCP server listening on {}", state.reaction_id, addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.wait_for(|stopped| *stopped).await;
        })
        .await?;
    Ok(())
}

async fn handle_post(
    State((state, shutdown)): State<HttpState>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    if !check_authorization(&headers, &state.config.bearer_token) {
        warn!("[{}] Unauthorized POST request rejected", state.reaction_id);
        return (
            StatusCode::UNAUTHORIZED,
            jsonrpc_error(None, -32000, "Unauthorized"),
        )
            .into_response();
    }
    if *shutdown.borrow() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            jsonrpc_error(None, -32000, "MCP server is stopping"),
        )
            .into_response();
    }

    let session_id = headers
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|s| s.len() <= MAX_SESSION_ID_LEN)
        .map(str::to_string);

    // JSON-RPC batch support: accept both single objects and arrays.
    match payload {
        Value::Array(items) => {
            debug!(
                "[{}] Received JSON-RPC batch with {} message(s)",
                state.reaction_id,
                items.len()
            );
            if items.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    jsonrpc_error(None, -32600, "Empty batch"),
                )
                    .into_response();
            }

            let mut responses: Vec<Value> = Vec::new();
            let mut session_header: Option<String> = None;

            for item in items {
                let request: JsonRpcRequest = match serde_json::from_value(item) {
                    Ok(req) => req,
                    Err(err) => {
                        warn!(
                            "[{}] Invalid JSON-RPC message in batch: {err}",
                            state.reaction_id
                        );
                        responses.push(
                            serde_json::to_value(JsonRpcResponse {
                                jsonrpc: "2.0",
                                id: None,
                                result: None,
                                error: Some(JsonRpcErrorObject {
                                    code: -32700,
                                    message: format!("Parse error: {err}"),
                                }),
                            })
                            .unwrap_or(Value::Null),
                        );
                        continue;
                    }
                };

                let is_notification = request.id.is_none();
                let result = handle_single_request(&state, &session_id, request).await;

                if let Some(sid) = result.session_id {
                    session_header = Some(sid);
                }

                if is_notification {
                    // Notifications do not produce a response in a batch.
                    continue;
                }

                if let Some(body) = result.body {
                    responses.push(body);
                }
            }

            if responses.is_empty() {
                // All items were notifications/responses — return 202.
                return StatusCode::ACCEPTED.into_response();
            }

            let mut response = Json(Value::Array(responses)).into_response();
            if let Some(sid) = session_header {
                if let Ok(header_value) = HeaderValue::from_str(&sid) {
                    response
                        .headers_mut()
                        .insert(HeaderName::from_static(MCP_SESSION_HEADER), header_value);
                }
            }
            response
        }
        _ => {
            // Single JSON-RPC message.
            let request: JsonRpcRequest = match serde_json::from_value(payload) {
                Ok(req) => req,
                Err(err) => {
                    warn!("[{}] Invalid JSON-RPC request: {err}", state.reaction_id);
                    return (
                        StatusCode::BAD_REQUEST,
                        jsonrpc_error(None, -32700, format!("Parse error: {err}")),
                    )
                        .into_response();
                }
            };

            let is_notification = request.id.is_none();
            let result = handle_single_request(&state, &session_id, request).await;

            if is_notification {
                return StatusCode::ACCEPTED.into_response();
            }

            let body = result.body.unwrap_or_else(|| {
                serde_json::to_value(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: None,
                    result: Some(json!({})),
                    error: None,
                })
                .unwrap_or(Value::Null)
            });

            let mut response = (result.status, Json(body)).into_response();
            if let Some(sid) = result.session_id {
                if let Ok(header_value) = HeaderValue::from_str(&sid) {
                    response
                        .headers_mut()
                        .insert(HeaderName::from_static(MCP_SESSION_HEADER), header_value);
                }
            }
            response
        }
    }
}

struct SingleRequestResult {
    body: Option<Value>,
    session_id: Option<String>,
    status: StatusCode,
}

async fn handle_single_request(
    state: &Arc<McpState>,
    session_id: &Option<String>,
    request: JsonRpcRequest,
) -> SingleRequestResult {
    match request.method.as_str() {
        "initialize" => {
            let params = request.params.clone().unwrap_or(Value::Null);
            let init_request: InitializeRequest = match serde_json::from_value(params) {
                Ok(req) => req,
                Err(err) => {
                    return SingleRequestResult {
                        body: Some(
                            serde_json::to_value(make_error(
                                request.id,
                                -32602,
                                format!("Invalid params: {err}"),
                            ))
                            .unwrap_or(Value::Null),
                        ),
                        session_id: None,
                        status: StatusCode::BAD_REQUEST,
                    };
                }
            };

            debug!(
                "[{}] Initialize request with protocol {}",
                state.reaction_id, init_request.protocol_version
            );

            // Protocol version negotiation per MCP spec:
            // echo back the client's version if supported, otherwise respond with our latest.
            const SUPPORTED_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];
            let negotiated_version =
                if SUPPORTED_VERSIONS.contains(&init_request.protocol_version.as_str()) {
                    info!(
                        "[{}] Negotiated protocol version: {}",
                        state.reaction_id, init_request.protocol_version
                    );
                    init_request.protocol_version.clone()
                } else {
                    warn!(
                    "[{}] Client requested unsupported protocol version '{}', responding with {}",
                    state.reaction_id,
                    init_request.protocol_version,
                    LATEST_PROTOCOL_VERSION.as_str()
                );
                    LATEST_PROTOCOL_VERSION.as_str().to_string()
                };

            let new_session_id = match session_id {
                Some(id) if state.sessions.read().await.contains_key(id) => id.clone(),
                _ => {
                    let mut sessions = state.sessions.write().await;
                    if sessions.len() >= state.config.max_sessions {
                        warn!(
                            "[{}] Maximum session limit ({}) reached, rejecting initialize",
                            state.reaction_id, state.config.max_sessions
                        );
                        return SingleRequestResult {
                            body: Some(
                                serde_json::to_value(make_error(
                                    request.id,
                                    -32000,
                                    "Maximum session limit reached",
                                ))
                                .unwrap_or(Value::Null),
                            ),
                            session_id: None,
                            status: StatusCode::SERVICE_UNAVAILABLE,
                        };
                    }

                    let sid = Uuid::new_v4().to_string();
                    let (tx, rx) = mpsc::channel(state.config.session_channel_capacity);
                    let session = SessionState {
                        sender: tx,
                        receiver: Arc::new(Mutex::new(Some(rx))),
                    };
                    sessions.insert(sid.clone(), session);
                    info!("[{}] New MCP session created: {sid}", state.reaction_id);
                    sid
                }
            };

            let response = InitializeResponse {
                protocol_version: negotiated_version,
                capabilities: ServerCapabilities {
                    resources: Some(ResourceCapabilities {
                        subscribe: Some(true),
                        list_changed: Some(true),
                    }),
                    ..ServerCapabilities::default()
                },
                server_info: Implementation {
                    name: "drasi-mcp-reaction".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                instructions: Some("Drasi MCP server providing query resources.".to_string()),
            };

            let body = make_result(
                request.id,
                serde_json::to_value(response).unwrap_or(Value::Null),
            );

            SingleRequestResult {
                body: Some(serde_json::to_value(body).unwrap_or(Value::Null)),
                session_id: Some(new_session_id),
                status: StatusCode::OK,
            }
        }
        _ => {
            let request_id = request.id.clone();
            let session_id = match session_id {
                Some(id) => id.clone(),
                None => {
                    warn!(
                        "[{}] Request '{}' rejected: missing session ID",
                        state.reaction_id, request.method
                    );
                    return SingleRequestResult {
                        body: Some(
                            serde_json::to_value(make_error(
                                request_id,
                                -32000,
                                "Missing session ID",
                            ))
                            .unwrap_or(Value::Null),
                        ),
                        session_id: None,
                        status: StatusCode::BAD_REQUEST,
                    };
                }
            };

            if !state.sessions.read().await.contains_key(&session_id) {
                warn!(
                    "[{}] Request '{}' rejected: invalid session ID {session_id}",
                    state.reaction_id, request.method
                );
                return SingleRequestResult {
                    body: Some(
                        serde_json::to_value(make_error(request_id, -32000, "Invalid session ID"))
                            .unwrap_or(Value::Null),
                    ),
                    session_id: None,
                    status: StatusCode::NOT_FOUND,
                };
            }

            debug!(
                "[{}] Handling '{}' for session {session_id}",
                state.reaction_id, request.method
            );

            let body = match request.method.as_str() {
                "resources/subscribe" => {
                    let uri = request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("uri"))
                        .and_then(Value::as_str);
                    let uri = match uri {
                        Some(uri) => uri,
                        None => {
                            warn!(
                                "[{}] resources/subscribe: missing URI param",
                                state.reaction_id
                            );
                            return SingleRequestResult {
                                body: Some(
                                    serde_json::to_value(make_error(
                                        request_id,
                                        -32602,
                                        "URI is required",
                                    ))
                                    .unwrap_or(Value::Null),
                                ),
                                session_id: None,
                                status: StatusCode::BAD_REQUEST,
                            };
                        }
                    };
                    if validate_subscription_uri(uri, &state.query_ids).is_none() {
                        warn!(
                            "[{}] resources/subscribe: invalid URI '{uri}' (known queries: {:?})",
                            state.reaction_id, state.query_ids
                        );
                        return SingleRequestResult {
                            body: Some(
                                serde_json::to_value(make_error(
                                    request_id,
                                    -32602,
                                    "Invalid URI (expected drasi://query/{known-query-id})",
                                ))
                                .unwrap_or(Value::Null),
                            ),
                            session_id: None,
                            status: StatusCode::BAD_REQUEST,
                        };
                    }
                    if !state.add_subscription(&session_id, uri).await {
                        return SingleRequestResult {
                            body: Some(
                                serde_json::to_value(make_error(
                                    request_id,
                                    -32000,
                                    "Invalid session ID",
                                ))
                                .unwrap_or(Value::Null),
                            ),
                            session_id: None,
                            status: StatusCode::NOT_FOUND,
                        };
                    }
                    make_result(request_id, json!({}))
                }
                "resources/unsubscribe" => {
                    let uri = request
                        .params
                        .as_ref()
                        .and_then(|params| params.get("uri"))
                        .and_then(Value::as_str);
                    let uri = match uri {
                        Some(uri) => uri,
                        None => {
                            return SingleRequestResult {
                                body: Some(
                                    serde_json::to_value(make_error(
                                        request_id,
                                        -32602,
                                        "URI is required",
                                    ))
                                    .unwrap_or(Value::Null),
                                ),
                                session_id: None,
                                status: StatusCode::BAD_REQUEST,
                            };
                        }
                    };
                    if validate_subscription_uri(uri, &state.query_ids).is_none() {
                        return SingleRequestResult {
                            body: Some(
                                serde_json::to_value(make_error(
                                    request_id,
                                    -32602,
                                    "Invalid URI (expected drasi://query/{known-query-id})",
                                ))
                                .unwrap_or(Value::Null),
                            ),
                            session_id: None,
                            status: StatusCode::BAD_REQUEST,
                        };
                    }
                    state.remove_subscription(&session_id, uri).await;
                    make_result(request_id, json!({}))
                }
                "resources/list" => {
                    let mut resources = Vec::new();
                    for query_id in &state.query_ids {
                        let uri = match resource_uri(query_id) {
                            Some(uri) => uri,
                            None => continue,
                        };
                        let query_config = get_query_config(query_id, &state.config.routes);
                        let (name, description) = match query_config {
                            Some(config) => (
                                config.title.clone().unwrap_or_else(|| query_id.clone()),
                                config.description.clone(),
                            ),
                            None => (query_id.clone(), None),
                        };
                        resources.push(Resource {
                            uri,
                            name,
                            description,
                            mime_type: Some("application/json".to_string()),
                            annotations: None,
                            size: None,
                        });
                    }

                    debug!(
                        "[{}] resources/list: returning {} resource(s)",
                        state.reaction_id,
                        resources.len()
                    );

                    let response = ResourcesListResponse {
                        resources,
                        next_cursor: None,
                        meta: None,
                    };
                    make_result(
                        request_id,
                        serde_json::to_value(response).unwrap_or(Value::Null),
                    )
                }
                "resources/read" => {
                    let params = request.params.clone().unwrap_or(Value::Null);
                    let read_request: ReadResourceRequest = match serde_json::from_value(params) {
                        Ok(req) => req,
                        Err(err) => {
                            warn!(
                                "[{}] resources/read: invalid params: {err}",
                                state.reaction_id
                            );
                            return SingleRequestResult {
                                body: Some(
                                    serde_json::to_value(make_error(
                                        request_id,
                                        -32602,
                                        format!("Invalid params: {err}"),
                                    ))
                                    .unwrap_or(Value::Null),
                                ),
                                session_id: None,
                                status: StatusCode::BAD_REQUEST,
                            };
                        }
                    };

                    let query_id = match read_request.uri.host_str() {
                        Some("query") => {
                            read_request.uri.path().trim_start_matches('/').to_string()
                        }
                        _ => String::new(),
                    };

                    if query_id.is_empty() || !state.query_ids.contains(&query_id) {
                        warn!(
                            "[{}] resources/read: unknown query '{query_id}' (known: {:?})",
                            state.reaction_id, state.query_ids
                        );
                        return SingleRequestResult {
                            body: Some(
                                serde_json::to_value(make_error(
                                    request_id,
                                    -32602,
                                    "Invalid URI format (expected drasi://query/{id})",
                                ))
                                .unwrap_or(Value::Null),
                            ),
                            session_id: None,
                            status: StatusCode::BAD_REQUEST,
                        };
                    }

                    let current = state.current_results.read().await;
                    let results = current.get(&query_id).cloned().unwrap_or_default();
                    debug!(
                        "[{}] resources/read: returning {} result(s) for '{query_id}'",
                        state.reaction_id,
                        results.len()
                    );
                    let contents = ResourceContents {
                        uri: read_request.uri.clone(),
                        mime_type: Some("application/json".to_string()),
                        text: Some(
                            serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".into()),
                        ),
                        blob: None,
                    };
                    let response = ReadResourceResponse {
                        contents: vec![contents],
                        meta: None,
                    };
                    make_result(
                        request_id,
                        serde_json::to_value(response).unwrap_or(Value::Null),
                    )
                }
                "notifications/initialized" => {
                    debug!(
                        "[{}] Client initialized acknowledgement received",
                        state.reaction_id
                    );
                    return SingleRequestResult {
                        body: None,
                        session_id: None,
                        status: StatusCode::ACCEPTED,
                    };
                }
                "ping" => {
                    debug!("[{}] Ping received", state.reaction_id);
                    make_result(request_id, json!({}))
                }
                unknown => {
                    warn!("[{}] Unknown method: '{unknown}'", state.reaction_id);
                    make_error(request_id, -32601, "Method not found")
                }
            };

            SingleRequestResult {
                body: Some(serde_json::to_value(body).unwrap_or(Value::Null)),
                session_id: None,
                status: StatusCode::OK,
            }
        }
    }
}

async fn handle_sse(
    State((state, shutdown)): State<HttpState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !check_authorization(&headers, &state.config.bearer_token) {
        warn!("[{}] Unauthorized SSE request rejected", state.reaction_id);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if *shutdown.borrow() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    let session_id = headers
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|s| s.len() <= MAX_SESSION_ID_LEN)
        .map(str::to_string);
    let session_id = match session_id {
        Some(id) => id,
        None => {
            debug!(
                "[{}] GET without session ID — returning 405 (SSE requires prior initialization)",
                state.reaction_id
            );
            return StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
    };

    let session = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).cloned()
    };

    let session = match session {
        Some(session) => session,
        None => {
            warn!(
                "[{}] SSE request rejected: unknown session {session_id}",
                state.reaction_id
            );
            return StatusCode::NOT_FOUND.into_response();
        }
    };

    let receiver = {
        let mut receiver_guard = session.receiver.lock().await;
        receiver_guard.take()
    };

    let receiver = match receiver {
        Some(receiver) => receiver,
        None => {
            warn!(
                "[{}] SSE request rejected: session {session_id} already has an active SSE stream",
                state.reaction_id
            );
            return StatusCode::CONFLICT.into_response();
        }
    };

    info!(
        "[{}] SSE stream opened for session {session_id}",
        state.reaction_id
    );

    let stream = SessionStream {
        inner: ReceiverStream::new(receiver),
        shutdown: WatchStream::new(shutdown),
        stopped: false,
        state: state.clone(),
        session_id,
    };

    Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::context::workers::WorkerCleanupError;
    use futures::{FutureExt, StreamExt};

    #[test]
    fn test_remove_first() {
        let mut values = vec![json!({"id": 1}), json!({"id": 2})];
        remove_first(&mut values, &json!({"id": 1}));
        assert_eq!(values, vec![json!({"id": 2})]);
    }

    #[test]
    fn test_auth_validation() {
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer secret-token"),
        );
        // Correct token → allowed
        assert!(check_authorization(
            &headers,
            &Some("secret-token".to_string())
        ));
        // Wrong token → denied
        assert!(!check_authorization(
            &headers,
            &Some("wrong-token".to_string())
        ));
        // Missing Authorization header → denied
        let empty_headers = HeaderMap::new();
        assert!(!check_authorization(
            &empty_headers,
            &Some("secret-token".to_string())
        ));
        // No token required (None) → any request allowed
        assert!(check_authorization(&headers, &None));
        assert!(check_authorization(&empty_headers, &None));
    }

    #[test]
    fn test_resource_uri_format() {
        let uri = resource_uri("test-query").expect("should build resource uri");
        assert_eq!(uri.scheme(), "drasi");
        assert_eq!(uri.host_str(), Some("query"));
        assert_eq!(uri.path(), "/test-query");
    }

    #[test]
    fn test_template_rendering() {
        let mut handlebars = Handlebars::new();
        register_json_helper(&mut handlebars);

        // Test ADD template
        let mut context = Map::new();
        context.insert("after".to_string(), json!({"id": 1}));
        let rendered = handlebars
            .render_template(DEFAULT_ADDED_TEMPLATE, &context)
            .expect("template should render");
        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["operation"], "added");
        assert_eq!(parsed["data"]["id"], 1);

        // Test UPDATE template
        let mut context = Map::new();
        context.insert("before".to_string(), json!({"id": 1, "name": "old"}));
        context.insert("after".to_string(), json!({"id": 1, "name": "new"}));
        let rendered = handlebars
            .render_template(DEFAULT_UPDATED_TEMPLATE, &context)
            .expect("update template should render");
        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["operation"], "updated");
        assert_eq!(parsed["before"]["name"], "old");
        assert_eq!(parsed["after"]["name"], "new");

        // Test DELETE template
        let mut context = Map::new();
        context.insert("before".to_string(), json!({"id": 1, "name": "deleted"}));
        let rendered = handlebars
            .render_template(DEFAULT_DELETED_TEMPLATE, &context)
            .expect("delete template should render");
        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["operation"], "deleted");
        assert_eq!(parsed["data"]["name"], "deleted");

        // Test custom template with json helper
        let custom_template = r#"{"result":{{json after}}}"#;
        let mut context = Map::new();
        context.insert("after".to_string(), json!({"complex": [1, 2, 3]}));
        let rendered = handlebars
            .render_template(custom_template, &context)
            .expect("custom template should render");
        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["result"]["complex"], json!([1, 2, 3]));
    }

    #[tokio::test]
    async fn test_subscription_tracking() {
        let state = McpState {
            reaction_id: "test".to_string(),
            config: McpReactionConfig::default(),
            query_ids: vec!["query1".to_string()],
            sessions: Arc::new(RwLock::new(HashMap::new())),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            subscribers_by_uri: Arc::new(RwLock::new(HashMap::new())),
            current_results: Arc::new(RwLock::new(HashMap::new())),
            session_cleanup: Arc::new(Notify::new()),
        };
        let (tx, _rx) = mpsc::channel(16);
        state.sessions.write().await.insert(
            "session1".into(),
            SessionState {
                sender: tx,
                receiver: Arc::new(Mutex::new(None)),
            },
        );

        let uri = "drasi://query/query1";

        // Add subscription
        assert!(state.add_subscription("session1", uri).await);
        {
            let subscriptions = state.subscriptions.read().await;
            assert!(subscriptions
                .get("session1")
                .is_some_and(|set| set.contains(uri)));
        }

        // Verify get_subscribers_for_uri (via subscribers_by_uri)
        {
            let subscribers = state.subscribers_by_uri.read().await;
            assert!(subscribers
                .get(uri)
                .is_some_and(|set| set.contains("session1")));
        }

        // Duplicate add should not create duplicate entries
        assert!(state.add_subscription("session1", uri).await);
        {
            let subscribers = state.subscribers_by_uri.read().await;
            assert_eq!(subscribers.get(uri).map(|s| s.len()), Some(1));
        }

        // Remove subscription
        state.remove_subscription("session1", uri).await;
        {
            let subscriptions = state.subscriptions.read().await;
            assert!(
                subscriptions.get("session1").is_none(),
                "session entry should be removed when empty"
            );
            let subscribers = state.subscribers_by_uri.read().await;
            assert!(
                subscribers.get(uri).is_none(),
                "uri entry should be removed when no subscribers"
            );
        }
    }

    #[tokio::test]
    async fn test_cleanup_session() {
        let state = McpState {
            reaction_id: "test".to_string(),
            config: McpReactionConfig::default(),
            query_ids: vec!["query1".to_string(), "query2".to_string()],
            sessions: Arc::new(RwLock::new(HashMap::new())),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            subscribers_by_uri: Arc::new(RwLock::new(HashMap::new())),
            current_results: Arc::new(RwLock::new(HashMap::new())),
            session_cleanup: Arc::new(Notify::new()),
        };

        // Create a session
        let (tx, _rx) = mpsc::channel(16);
        state.sessions.write().await.insert(
            "session1".to_string(),
            SessionState {
                sender: tx,
                receiver: Arc::new(Mutex::new(None)),
            },
        );

        // Add subscriptions
        state
            .add_subscription("session1", "drasi://query/query1")
            .await;
        state
            .add_subscription("session1", "drasi://query/query2")
            .await;

        // Cleanup session
        state.cleanup_session("session1").await;

        // Verify everything is cleaned up
        assert!(state.sessions.read().await.is_empty());
        assert!(state.subscriptions.read().await.is_empty());
        assert!(state.subscribers_by_uri.read().await.is_empty());
    }

    #[test]
    fn test_validate_subscription_uri() {
        let query_ids = vec!["query1".to_string(), "query2".to_string()];

        // Valid URIs
        assert_eq!(
            validate_subscription_uri("drasi://query/query1", &query_ids),
            Some("query1")
        );
        assert_eq!(
            validate_subscription_uri("drasi://query/query2", &query_ids),
            Some("query2")
        );

        // Invalid: unknown query
        assert_eq!(
            validate_subscription_uri("drasi://query/unknown", &query_ids),
            None
        );

        // Invalid: wrong prefix
        assert_eq!(
            validate_subscription_uri("http://query/query1", &query_ids),
            None
        );

        // Invalid: empty query id
        assert_eq!(
            validate_subscription_uri("drasi://query/", &query_ids),
            None
        );

        // Invalid: no prefix match
        assert_eq!(validate_subscription_uri("query1", &query_ids), None);
    }

    fn test_state_with_session() -> (Arc<McpState>, String) {
        let session_id = "test-session-id".to_string();
        let (tx, rx) = mpsc::channel(16);
        let mut sessions = HashMap::new();
        sessions.insert(
            session_id.clone(),
            SessionState {
                sender: tx,
                receiver: Arc::new(Mutex::new(Some(rx))),
            },
        );
        let state = Arc::new(McpState {
            reaction_id: "test".to_string(),
            config: McpReactionConfig::default(),
            query_ids: vec!["query1".to_string()],
            sessions: Arc::new(RwLock::new(sessions)),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            subscribers_by_uri: Arc::new(RwLock::new(HashMap::new())),
            current_results: Arc::new(RwLock::new(HashMap::new())),
            session_cleanup: Arc::new(Notify::new()),
        });
        (state, session_id)
    }

    #[test]
    fn stream_drop_closes_admission_without_spawning_a_cleanup_task() {
        let (state, session_id) = test_state_with_session();
        let session = state
            .sessions
            .try_read()
            .unwrap()
            .get(&session_id)
            .unwrap()
            .clone();
        let receiver = session.receiver.try_lock().unwrap().take().unwrap();
        let (_shutdown, shutdown_rx) = watch::channel(false);
        drop(SessionStream {
            inner: ReceiverStream::new(receiver),
            shutdown: WatchStream::new(shutdown_rx),
            stopped: false,
            state: state.clone(),
            session_id,
        });
        assert!(session.sender.is_closed());
        assert!(state.session_cleanup.notified().now_or_never().is_some());
        assert_eq!(state.sessions.try_read().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_ends_buffered_streams_and_rejects_closed_session_subscriptions() {
        let (state, session_id) = test_state_with_session();
        assert!(
            state
                .add_subscription(&session_id, "drasi://query/query1")
                .await
        );
        let session = state
            .sessions
            .read()
            .await
            .get(&session_id)
            .unwrap()
            .clone();
        let receiver = session.receiver.lock().await.take().unwrap();
        let (shutdown, shutdown_rx) = watch::channel(false);
        let mut stream = SessionStream {
            inner: ReceiverStream::new(receiver),
            shutdown: WatchStream::new(shutdown_rx),
            stopped: false,
            state: state.clone(),
            session_id: session_id.clone(),
        };
        session.sender.try_send("buffered".into()).unwrap();
        shutdown.send_replace(true);
        assert!(stream.next().await.is_none());
        assert!(stream.next().await.is_none());
        let result = handle_single_request(
            &state,
            &Some(session_id.clone()),
            JsonRpcRequest {
                jsonrpc: "2.0".into(),
                id: Some(json!(1)),
                method: "resources/subscribe".into(),
                params: Some(json!({"uri": "drasi://query/query1"})),
            },
        )
        .await;
        assert_eq!(result.status, StatusCode::NOT_FOUND);
        drop(stream);
        state.cleanup_closed_sessions().await;
        assert!(state.sessions.read().await.is_empty());
        assert!(state.subscriptions.read().await.is_empty());
        assert!(state.subscribers_by_uri.read().await.is_empty());
        assert!(
            !state
                .add_subscription(&session_id, "drasi://query/query1")
                .await
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bind_failure_is_typed_and_running_means_the_listener_is_bound() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let reaction = McpReaction::builder("occupied")
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
        assert_eq!(reaction.bound_port_handle().load(Ordering::SeqCst), 0);
        assert!(reaction.server_task.read().await.is_none());
        assert!(reaction.base.processing_task.read().await.is_none());
        drop(listener);
        reaction.start().await?;
        assert_ne!(reaction.bound_port_handle().load(Ordering::SeqCst), 0);
        reaction.stop().await?;
        assert_eq!(reaction.bound_port_handle().load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_cleanup_retains_worker_connection_and_registry_ownership() -> Result<()> {
        for boundary in ["worker", "connection", "registry"] {
            let reaction = McpReaction::builder("cancelled").with_port(0).build()?;
            *reaction.cleanup_required.lock().await = true;
            let release = Arc::new(Notify::new());
            let connection = if boundary == "connection" {
                Some(reaction.shutdown_tx.subscribe())
            } else {
                None
            };
            let registry = if boundary == "registry" {
                Some(reaction.subscriptions.write().await)
            } else {
                None
            };
            if boundary == "worker" {
                let drain = release.clone();
                spawn_owned_worker(&reaction.server_task, async move {
                    drain.notified().await;
                    Ok(())
                })
                .await?;
            }
            {
                let stop = reaction.stop();
                tokio::pin!(stop);
                tokio::select! {
                    result = &mut stop => panic!("cleanup remains incomplete: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
            assert_eq!(
                reaction.server_task.read().await.is_some(),
                boundary == "worker"
            );
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .downcast_ref::<WorkerAlreadyOwned>()
                .is_some());
            release.notify_one();
            drop(connection);
            drop(registry);
            reaction.stop().await?;
            reaction.start().await?;
            reaction.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_failures_preserve_typed_causes_and_require_remaining_cleanup() -> Result<()> {
        for panic in [false, true] {
            let reaction = McpReaction::builder("failed").with_port(0).build()?;
            *reaction.cleanup_required.lock().await = true;
            spawn_owned_worker(&reaction.server_task, async move {
                assert!(!panic, "injected MCP server panic");
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
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .downcast_ref::<WorkerAlreadyOwned>()
                .is_some());
            reaction.stop().await?;
            reaction.start().await?;
            reaction.stop().await?;
        }
        Ok(())
    }

    async fn initialize_session(client: &reqwest::Client, url: &str) -> Result<String> {
        let response = client
            .post(url)
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26", "capabilities": {},
                    "clientInfo": {"name": "lifecycle", "version": "1"}
                }
            }))
            .send()
            .await?
            .error_for_status()?;
        let id = response
            .headers()
            .get(MCP_SESSION_HEADER)
            .context("missing session")?
            .to_str()?
            .to_string();
        let body: Value = response.json().await?;
        assert!(body.get("error").is_none());
        Ok(id)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn disconnect_cleanup_and_restarts_reclaim_sessions_and_live_connections() -> Result<()> {
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = reservation.local_addr()?.port();
        drop(reservation);
        let reaction = McpReaction::builder("restart")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_query("query1")
            .with_max_sessions(2)
            .build()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        let url = format!("http://127.0.0.1:{port}/");
        reaction.stop().await?;
        for _ in 0..3 {
            reaction.start().await?;
            let session_id = initialize_session(&client, &url).await?;
            let idle_session = initialize_session(&client, &url).await?;
            let response = client
                .get(&url)
                .header(MCP_SESSION_HEADER, &session_id)
                .send()
                .await?
                .error_for_status()?;
            let mut stream = response.bytes_stream();
            let subscribed: Value = client
                .post(&url)
                .header(MCP_SESSION_HEADER, &session_id)
                .json(&json!({
                    "jsonrpc": "2.0", "id": 2, "method": "resources/subscribe",
                    "params": {"uri": "drasi://query/query1"}
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            assert!(subscribed.get("error").is_none());
            reaction
                .sessions
                .read()
                .await
                .get(&session_id)
                .expect("live session")
                .sender
                .try_send("first".into())?;
            assert!(
                std::str::from_utf8(&stream.next().await.context("live stream")??)?
                    .contains("first")
            );
            drop(stream);
            tokio::time::timeout(Duration::from_secs(2), async {
                while reaction.sessions.read().await.contains_key(&session_id)
                    || reaction
                        .subscriptions
                        .read()
                        .await
                        .contains_key(&session_id)
                    || !reaction.subscribers_by_uri.read().await.is_empty()
                {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            assert!(!reaction
                .subscriptions
                .read()
                .await
                .contains_key(&session_id));
            assert!(reaction.subscribers_by_uri.read().await.is_empty());
            assert!(reaction.sessions.read().await.contains_key(&idle_session));
            let replacement = initialize_session(&client, &url).await?;
            let response = client
                .get(&url)
                .header(MCP_SESSION_HEADER, replacement)
                .send()
                .await?
                .error_for_status()?;
            let mut stream = response.bytes_stream();
            reaction.stop().await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(message) = stream.next().await {
                    message?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            assert!(reaction.sessions.read().await.is_empty());
            assert!(reaction.subscriptions.read().await.is_empty());
            assert!(reaction.subscribers_by_uri.read().await.is_empty());
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.server_task.read().await.is_none());
            assert_eq!(reaction.shutdown_tx.receiver_count(), 0);
            assert_eq!(reaction.bound_port_handle().load(Ordering::SeqCst), 0);
            let rebound = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            drop(rebound);
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn incomplete_http_requests_retain_the_server_until_a_later_stop() -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let reaction = McpReaction::builder("partial-http")
            .with_host("127.0.0.1")
            .with_port(0)
            .build()?;
        reaction.start().await?;
        let port = reaction.bound_port();
        let owners = reaction.shutdown_tx.receiver_count();
        let mut request = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        request.write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{").await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while reaction.shutdown_tx.receiver_count() <= owners {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        initialize_session(&client, &format!("http://127.0.0.1:{port}/")).await?;
        let error = reaction.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(WorkerCleanupError::TimedOut { timeout }) if *timeout == Duration::from_secs(5))
        );
        assert!(reaction.server_task.read().await.is_some());
        assert_ne!(reaction.status().await, ComponentStatus::Stopped);
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        drop(request);
        reaction.stop().await?;
        assert_eq!(reaction.shutdown_tx.receiver_count(), 0);
        let rebound = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
        drop(rebound);
        Ok(())
    }

    #[tokio::test]
    async fn test_version_negotiation_supported() {
        let (state, _) = test_state_with_session();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "initialize".to_string(),
            params: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" }
            })),
        };

        let result = handle_single_request(&state, &None, request).await;
        assert_eq!(result.status, StatusCode::OK);
        let body = result.body.expect("should have body");
        assert_eq!(body["result"]["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn test_version_negotiation_unsupported() {
        let (state, _) = test_state_with_session();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "initialize".to_string(),
            params: Some(json!({
                "protocolVersion": "1999-01-01",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" }
            })),
        };

        let result = handle_single_request(&state, &None, request).await;
        assert_eq!(result.status, StatusCode::OK);
        let body = result.body.expect("should have body");
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
    }

    #[tokio::test]
    async fn test_expired_session_returns_404() {
        let (state, _) = test_state_with_session();
        let bad_session = Some("nonexistent-session".to_string());
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "resources/list".to_string(),
            params: None,
        };

        let result = handle_single_request(&state, &bad_session, request).await;
        assert_eq!(result.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_notification_returns_accepted() {
        let (state, session_id) = test_state_with_session();
        let sid = Some(session_id);
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: "notifications/initialized".to_string(),
            params: None,
        };

        let result = handle_single_request(&state, &sid, request).await;
        assert_eq!(result.status, StatusCode::ACCEPTED);
        assert!(result.body.is_none());
    }

    #[tokio::test]
    async fn test_initialize_returns_session_id() {
        let (state, _) = test_state_with_session();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "initialize".to_string(),
            params: Some(json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "1.0" }
            })),
        };

        let result = handle_single_request(&state, &None, request).await;
        assert_eq!(result.status, StatusCode::OK);
        assert!(result.session_id.is_some(), "should assign session ID");
    }
}
