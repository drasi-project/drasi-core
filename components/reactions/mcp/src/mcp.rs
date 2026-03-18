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

use anyhow::Result;
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
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tower_http::cors::{Any, CorsLayer};
use url::Url;
use uuid::Uuid;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
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
}

impl McpState {
    async fn add_subscription(&self, session_id: &str, uri: &str) {
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
    state: Arc<McpState>,
    session_id: String,
}

impl Stream for SessionStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(message)) => Poll::Ready(Some(Ok(Event::default().data(message)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        let state = self.state.clone();
        let session_id = self.session_id.clone();
        info!(
            "[{}] SSE stream closed for session {session_id}",
            state.reaction_id
        );
        tokio::spawn(async move {
            state.cleanup_session(&session_id).await;
        });
    }
}

/// MCP reaction exposes Drasi query results via MCP protocol.
pub struct McpReaction {
    base: ReactionBase,
    config: McpReactionConfig,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    subscriptions: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    subscribers_by_uri: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    current_results: Arc<RwLock<HashMap<String, Vec<Value>>>>,
    server_task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
            server_task: Arc::new(Mutex::new(None)),
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
        let mut props = HashMap::new();
        props.insert(
            "port".to_string(),
            Value::Number((self.bound_port() as u64).into()),
        );
        props.insert(
            "requires_auth".to_string(),
            Value::Bool(self.config.bearer_token.is_some()),
        );
        props
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
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting MCP reaction".to_string()),
            )
            .await?;

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("MCP reaction started".to_string()),
            )
            .await?;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        let reaction_id = self.base.id.clone();
        let config = self.config.clone();
        let query_ids = self.base.queries.clone();
        let sessions = self.sessions.clone();
        let subscriptions = self.subscriptions.clone();
        let subscribers_by_uri = self.subscribers_by_uri.clone();
        let current_results = self.current_results.clone();
        let bound_port = self.bound_port.clone();

        let server_task = tokio::spawn(async move {
            let state = Arc::new(McpState {
                reaction_id,
                config,
                query_ids,
                sessions,
                subscriptions,
                subscribers_by_uri,
                current_results,
            });

            if let Err(error) = run_server(state, bound_port).await {
                error!("MCP server failed: {error}");
            }
        });

        *self.server_task.lock().await = Some(server_task);

        let status = self.base.status.clone();
        let subscriptions = self.subscriptions.clone();
        let subscribers_by_uri = self.subscribers_by_uri.clone();
        let sessions = self.sessions.clone();
        let priority_queue = self.base.priority_queue.clone();
        let current_results = self.current_results.clone();
        let reaction_id = self.base.id.clone();
        let query_configs = self.config.routes.clone();

        let processing_handle = tokio::spawn(async move {
            info!("[{reaction_id}] MCP result processing task started");
            let mut handlebars = Handlebars::new();
            register_json_helper(&mut handlebars);

            loop {
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                let query_result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                        break;
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
                    apply_diff(&query_id, diff, &current_results).await;

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
                        ResultDiff::Add { data } => {
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
                        ResultDiff::Delete { data } => {
                            context.insert("before".to_string(), data.clone());
                        }
                        ResultDiff::Aggregation { before, after } => {
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

                    let payload: Value = match serde_json::from_str(&rendered) {
                        Ok(value) => value,
                        Err(err) => {
                            warn!(
                                "[{reaction_id}] Template output was not valid JSON for {query_id}: {err}"
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
                        let subscribers = subscribers_by_uri.read().await;
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
                        let sessions_guard = sessions.read().await;
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
                        cleanup_session_maps(
                            &session_id,
                            &sessions,
                            &subscriptions,
                            &subscribers_by_uri,
                        )
                        .await;
                    }
                }
            }
        });

        self.base.set_processing_task(processing_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping MCP reaction", self.base.id);
        let session_count = self.sessions.read().await.len();
        if session_count > 0 {
            info!(
                "[{}] Disconnecting {session_count} active session(s)",
                self.base.id
            );
        }

        if let Some(task) = self.server_task.lock().await.take() {
            task.abort();
        }
        self.base.stop_common().await?;
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("MCP reaction stopped".into()),
            )
            .await?;
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
        ResultDiff::Add { data } => entry.push(data.clone()),
        ResultDiff::Delete { data } => remove_first(entry, data),
        ResultDiff::Update { before, after, .. } => {
            remove_first(entry, before);
            entry.push(after.clone());
        }
        ResultDiff::Aggregation { before, after } => {
            if let Some(before) = before {
                remove_first(entry, before);
            }
            entry.push(after.clone());
        }
        ResultDiff::Noop => {}
    }
}

fn remove_first(list: &mut Vec<Value>, target: &Value) {
    if let Some(index) = list.iter().position(|item| item == target) {
        list.remove(index);
    }
}

async fn cleanup_session_maps(
    session_id: &str,
    sessions: &Arc<RwLock<HashMap<String, SessionState>>>,
    subscriptions: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
    subscribers_by_uri: &Arc<RwLock<HashMap<String, HashSet<String>>>>,
) {
    debug!("Cleaning up disconnected session {session_id}");
    sessions.write().await.remove(session_id);
    let mut subscriptions_guard = subscriptions.write().await;
    if let Some(uris) = subscriptions_guard.remove(session_id) {
        let mut subscribers_guard = subscribers_by_uri.write().await;
        for uri in uris {
            if let Some(set) = subscribers_guard.get_mut(&uri) {
                set.remove(session_id);
                if set.is_empty() {
                    subscribers_guard.remove(&uri);
                }
            }
        }
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

async fn run_server(state: Arc<McpState>, bound_port: Arc<AtomicU16>) -> Result<()> {
    let app = Router::new()
        .route("/", post(handle_post).get(handle_sse))
        .with_state(state.clone())
        .layer(
            CorsLayer::new()
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers(Any)
                .allow_origin(Any),
        );

    let listener =
        tokio::net::TcpListener::bind((state.config.host.as_str(), state.config.port)).await?;
    let addr = listener.local_addr()?;
    bound_port.store(addr.port(), Ordering::SeqCst);

    info!("[{}] MCP server listening on {}", state.reaction_id, addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_post(
    State(state): State<Arc<McpState>>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    if !check_authorization(&headers, &state.config.bearer_token) {
        warn!(
            "[{}] Unauthorized POST request rejected",
            state.reaction_id
        );
        return (
            StatusCode::UNAUTHORIZED,
            jsonrpc_error(None, -32000, "Unauthorized"),
        )
            .into_response();
    }

    let session_id = headers
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
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
                    warn!(
                        "[{}] Invalid JSON-RPC request: {err}",
                        state.reaction_id
                    );
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

            let body = result
                .body
                .unwrap_or_else(|| serde_json::to_value(JsonRpcResponse {
                    jsonrpc: "2.0",
                    id: None,
                    result: Some(json!({})),
                    error: None,
                })
                .unwrap_or(Value::Null));

            let mut response = Json(body).into_response();
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
                        body: Some(serde_json::to_value(make_error(
                            request.id,
                            -32602,
                            format!("Invalid params: {err}"),
                        ))
                        .unwrap_or(Value::Null)),
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
                    let sessions = state.sessions.read().await;
                    if sessions.len() >= state.config.max_sessions {
                        warn!(
                            "[{}] Maximum session limit ({}) reached, rejecting initialize",
                            state.reaction_id, state.config.max_sessions
                        );
                        return SingleRequestResult {
                            body: Some(serde_json::to_value(make_error(
                                request.id,
                                -32000,
                                "Maximum session limit reached",
                            ))
                            .unwrap_or(Value::Null)),
                            session_id: None,
                            status: StatusCode::SERVICE_UNAVAILABLE,
                        };
                    }
                    drop(sessions);

                    let sid = Uuid::new_v4().to_string();
                    let (tx, rx) = mpsc::channel(state.config.session_channel_capacity);
                    let session = SessionState {
                        sender: tx,
                        receiver: Arc::new(Mutex::new(Some(rx))),
                    };
                    state
                        .sessions
                        .write()
                        .await
                        .insert(sid.clone(), session);
                    info!(
                        "[{}] New MCP session created: {sid}",
                        state.reaction_id
                    );
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
                        body: Some(serde_json::to_value(make_error(
                            request_id,
                            -32000,
                            "Missing session ID",
                        ))
                        .unwrap_or(Value::Null)),
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
                    body: Some(serde_json::to_value(make_error(
                        request_id,
                        -32000,
                        "Invalid session ID",
                    ))
                    .unwrap_or(Value::Null)),
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
                                body: Some(serde_json::to_value(make_error(
                                    request_id,
                                    -32602,
                                    "URI is required",
                                ))
                                .unwrap_or(Value::Null)),
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
                            body: Some(serde_json::to_value(make_error(
                                request_id,
                                -32602,
                                "Invalid URI (expected drasi://query/{known-query-id})",
                            ))
                            .unwrap_or(Value::Null)),
                            session_id: None,
                            status: StatusCode::BAD_REQUEST,
                        };
                    }
                    state.add_subscription(&session_id, uri).await;
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
                                body: Some(serde_json::to_value(make_error(
                                    request_id,
                                    -32602,
                                    "URI is required",
                                ))
                                .unwrap_or(Value::Null)),
                                session_id: None,
                                status: StatusCode::BAD_REQUEST,
                            };
                        }
                    };
                    if validate_subscription_uri(uri, &state.query_ids).is_none() {
                        return SingleRequestResult {
                            body: Some(serde_json::to_value(make_error(
                                request_id,
                                -32602,
                                "Invalid URI (expected drasi://query/{known-query-id})",
                            ))
                            .unwrap_or(Value::Null)),
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
                                body: Some(serde_json::to_value(make_error(
                                    request_id,
                                    -32602,
                                    format!("Invalid params: {err}"),
                                ))
                                .unwrap_or(Value::Null)),
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
                            body: Some(serde_json::to_value(make_error(
                                request_id,
                                -32602,
                                "Invalid URI format (expected drasi://query/{id})",
                            ))
                            .unwrap_or(Value::Null)),
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
                    warn!(
                        "[{}] Unknown method: '{unknown}'",
                        state.reaction_id
                    );
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

async fn handle_sse(State(state): State<Arc<McpState>>, headers: HeaderMap) -> impl IntoResponse {
    if !check_authorization(&headers, &state.config.bearer_token) {
        warn!("[{}] Unauthorized SSE request rejected", state.reaction_id);
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let session_id = headers
        .get(MCP_SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let session_id = match session_id {
        Some(id) => id,
        None => {
            warn!(
                "[{}] SSE request rejected: missing session ID header",
                state.reaction_id
            );
            return StatusCode::BAD_REQUEST.into_response();
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
        assert!(check_authorization(
            &headers,
            &Some("secret-token".to_string())
        ));
        assert!(!check_authorization(
            &headers,
            &Some("wrong-token".to_string())
        ));
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
        let mut context = Map::new();
        context.insert("after".to_string(), json!({"id": 1}));
        let rendered = handlebars
            .render_template(DEFAULT_ADDED_TEMPLATE, &context)
            .expect("template should render");
        let parsed: Value = serde_json::from_str(&rendered).expect("valid json");
        assert_eq!(parsed["operation"], "added");
        assert_eq!(parsed["data"]["id"], 1);
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
        };

        state
            .add_subscription("session1", "drasi://query/query1")
            .await;

        let subscriptions = state.subscriptions.read().await;
        assert!(subscriptions
            .get("session1")
            .is_some_and(|set| set.contains("drasi://query/query1")));
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
        });
        (state, session_id)
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
