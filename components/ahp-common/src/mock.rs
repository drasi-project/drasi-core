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

//! A small in-process AHP host served over WebSocket, for tests and demos.
//!
//! It implements the subset of the protocol the Drasi AHP plugins use
//! (`initialize`, `reconnect`, `subscribe`, `unsubscribe`, `listSessions`,
//! `authenticate`, `ping`) over the real wire format. State changes are
//! made through [`MockAhpHost::dispatch`], which runs the official `ahp`
//! reducers, broadcasts `action` envelopes to subscribers, and keeps the
//! session catalogue current with `root/session*` notifications — exactly as
//! a real host would.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ahp::reducers::{apply_action_to_automation, apply_action_to_root, apply_action_to_session};
use ahp_types::actions::{ActionEnvelope, StateAction};
use ahp_types::commands::{InitializeParams, ListSessionsParams, ReconnectParams, SubscribeParams};
use ahp_types::messages::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcSuccessResponse, JsonRpcVersion,
};
use ahp_types::notifications::PartialSessionSummary;
use ahp_types::state::{
    AgentInfo, AutomationState, ChangesSummary, RootState, SessionChatSummary, SessionState,
    SessionSummary, Snapshot, SnapshotState,
};
use ahp_types::ROOT_RESOURCE_URI;
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use crate::model::AUTOMATIONS_URI;

struct Connection {
    tx: mpsc::UnboundedSender<Message>,
    subscriptions: HashSet<String>,
    task: Option<JoinHandle<()>>,
}

struct HostState {
    seq: i64,
    root: RootState,
    sessions: BTreeMap<String, (SessionSummary, SessionState)>,
    automations: Option<AutomationState>,
    connections: HashMap<u64, Connection>,
    next_connection: u64,
    bearer_token: Option<String>,
}

impl HostState {
    fn snapshot(&self, uri: &str) -> Option<Snapshot> {
        let state = if uri == ROOT_RESOURCE_URI {
            SnapshotState::Root(Box::new(self.root.clone()))
        } else if uri == AUTOMATIONS_URI {
            SnapshotState::Automations(Box::new(self.automations.clone()?))
        } else {
            SnapshotState::Session(Box::new(self.sessions.get(uri)?.1.clone()))
        };
        Some(Snapshot {
            resource: uri.to_string(),
            state,
            from_seq: self.seq,
        })
    }

    fn broadcast(&mut self, channel: &str, method: &str, params: Value) {
        let text = notification_text(method, params);
        self.connections.retain(|_, c| {
            if !c.subscriptions.contains(channel) {
                return true;
            }
            c.tx.send(Message::Text(text.clone())).is_ok()
        });
    }

    fn broadcast_action(&mut self, channel: &str, action: StateAction) {
        self.seq += 1;
        let envelope = ActionEnvelope {
            channel: channel.to_string(),
            action,
            server_seq: u64::try_from(self.seq).unwrap_or_default(),
            origin: None,
            rejection_reason: None,
        };
        let params = serde_json::to_value(&envelope).unwrap_or(Value::Null);
        self.broadcast(channel, "action", params);
    }
}

/// In-process AHP host listening on a local WebSocket port.
#[derive(Clone)]
pub struct MockAhpHost {
    addr: SocketAddr,
    state: Arc<Mutex<HostState>>,
    accept_task: Arc<JoinHandle<()>>,
}

impl Drop for MockAhpHost {
    fn drop(&mut self) {
        if Arc::strong_count(&self.accept_task) == 1 {
            self.accept_task.abort();
            self.disconnect_all();
        }
    }
}

fn lock(state: &Mutex<HostState>) -> std::sync::MutexGuard<'_, HostState> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn notification_text(method: &str, params: Value) -> String {
    serde_json::to_string(&JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: JsonRpcVersion::V2,
        method: method.to_string(),
        params: Some(params),
    }))
    .unwrap_or_default()
}

fn summary_from_state(
    uri: &str,
    state: &SessionState,
    previous: Option<&SessionSummary>,
) -> SessionSummary {
    let now = now_iso();
    SessionSummary {
        provider: state.provider.clone(),
        title: state.title.clone(),
        status: state.status,
        activity: state.activity.clone(),
        origin: state.origin.clone(),
        project: state.project.clone(),
        working_directories: state.working_directories.clone(),
        annotations: state.annotations.clone(),
        resource: uri.to_string(),
        created_at: previous.map_or_else(|| now.clone(), |p| p.created_at.clone()),
        modified_at: now,
        changes: previous.and_then(|p| p.changes.clone()),
        meta: None,
        chats: Some(
            state
                .chats
                .iter()
                .map(|c| SessionChatSummary {
                    resource: c.resource.clone(),
                    title: c.title.clone(),
                    origin: c.origin.clone(),
                    interactivity: c.interactivity,
                    provider: c.provider.clone(),
                    status: Some(c.status),
                    changes: c.changes.clone(),
                })
                .collect(),
        ),
        default_chat: state.default_chat.clone(),
    }
}

/// Partial summary with only the fields that differ.
fn summary_delta(old: &SessionSummary, new: &SessionSummary) -> PartialSessionSummary {
    let mut delta = PartialSessionSummary::default();
    if old.title != new.title {
        delta.title = Some(new.title.clone());
    }
    if old.status != new.status {
        delta.status = Some(new.status);
    }
    if old.activity != new.activity {
        delta.activity = new.activity.clone().or(Some(String::new()));
    }
    if old.chats != new.chats {
        delta.chats = new.chats.clone();
    }
    if old.default_chat != new.default_chat {
        delta.default_chat = new.default_chat.clone();
    }
    if old.changes != new.changes {
        delta.changes = new.changes.clone();
    }
    if old.project != new.project {
        delta.project = new.project.clone();
    }
    delta.modified_at = Some(new.modified_at.clone());
    delta
}

impl MockAhpHost {
    /// Start a host on an ephemeral local port.
    pub async fn start() -> Result<Self> {
        Self::start_on("127.0.0.1:0").await
    }

    /// Start a host on the given address.
    pub async fn start_on(addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(HostState {
            seq: 0,
            root: RootState {
                agents: Vec::new(),
                active_sessions: Some(0),
                terminals: None,
                config: None,
                meta: None,
            },
            sessions: BTreeMap::new(),
            automations: None,
            connections: HashMap::new(),
            next_connection: 1,
            bearer_token: None,
        }));
        let accept_state = state.clone();
        let accept_task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = accept_state.clone();
                tokio::spawn(async move {
                    let _ = serve_connection(stream, state).await;
                });
            }
        });
        Ok(Self {
            addr,
            state,
            accept_task: Arc::new(accept_task),
        })
    }

    /// `ws://` URL clients should connect to.
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Require `Authorization: Bearer <token>` on new connections.
    pub fn require_bearer_token(&self, token: Option<String>) {
        lock(&self.state).bearer_token = token;
    }

    /// Advertise the automations capability and serve `ahp-automations://`.
    pub fn enable_automations(&self, catalogue: AutomationState) {
        lock(&self.state).automations = Some(catalogue);
    }

    /// Number of open client connections.
    pub fn connection_count(&self) -> usize {
        lock(&self.state).connections.len()
    }

    /// Replace the advertised agents (`root/agentsChanged`).
    pub fn set_agents(&self, agents: Vec<AgentInfo>) -> Result<()> {
        let action: StateAction =
            serde_json::from_value(json!({"type": "root/agentsChanged", "agents": agents}))?;
        self.dispatch(ROOT_RESOURCE_URI, action)
    }

    /// Create a session and announce it with `root/sessionAdded`.
    pub fn add_session(&self, uri: &str, state: SessionState) {
        let mut host = lock(&self.state);
        let summary = summary_from_state(uri, &state, None);
        host.sessions
            .insert(uri.to_string(), (summary.clone(), state));
        let count = host.sessions.len() as i64;
        host.root.active_sessions = Some(count);
        host.broadcast(
            ROOT_RESOURCE_URI,
            "root/sessionAdded",
            json!({"channel": ROOT_RESOURCE_URI, "summary": summary}),
        );
    }

    /// Dispose a session and announce it with `root/sessionRemoved`.
    pub fn remove_session(&self, uri: &str) {
        let mut host = lock(&self.state);
        if host.sessions.remove(uri).is_some() {
            host.broadcast(
                ROOT_RESOURCE_URI,
                "root/sessionRemoved",
                json!({"channel": ROOT_RESOURCE_URI, "session": uri}),
            );
            for connection in host.connections.values_mut() {
                connection.subscriptions.remove(uri);
            }
        }
    }

    /// Update the aggregate file-change counts in a session summary.
    pub fn set_session_changes(&self, uri: &str, changes: ChangesSummary) {
        let mut host = lock(&self.state);
        let Some((summary, _)) = host.sessions.get_mut(uri) else {
            return;
        };
        summary.changes = Some(changes.clone());
        summary.modified_at = now_iso();
        let delta = PartialSessionSummary {
            changes: Some(changes),
            modified_at: Some(summary.modified_at.clone()),
            ..Default::default()
        };
        host.broadcast(
            ROOT_RESOURCE_URI,
            "root/sessionSummaryChanged",
            json!({"channel": ROOT_RESOURCE_URI, "session": uri, "changes": delta}),
        );
    }

    /// Set a session's aggregate status bitset (e.g. `Error`), as a host does
    /// when a turn ends, and announce it with `root/sessionSummaryChanged`.
    pub fn set_session_status(&self, uri: &str, status: u32) {
        let mut host = lock(&self.state);
        let Some((summary, state)) = host.sessions.get_mut(uri) else {
            return;
        };
        state.status = status;
        summary.status = status;
        summary.modified_at = now_iso();
        let delta = PartialSessionSummary {
            status: Some(status),
            modified_at: Some(summary.modified_at.clone()),
            ..Default::default()
        };
        host.broadcast(
            ROOT_RESOURCE_URI,
            "root/sessionSummaryChanged",
            json!({"channel": ROOT_RESOURCE_URI, "session": uri, "changes": delta}),
        );
    }

    /// Apply `action` to the channel's state with the official reducers and
    /// broadcast it. Session actions also refresh the catalogue summary.
    pub fn dispatch(&self, channel: &str, action: StateAction) -> Result<()> {
        let mut host = lock(&self.state);
        if channel == ROOT_RESOURCE_URI {
            apply_action_to_root(&mut host.root, &action);
            host.broadcast_action(channel, action);
            return Ok(());
        }
        if channel == AUTOMATIONS_URI {
            let automations = host
                .automations
                .as_mut()
                .ok_or_else(|| anyhow!("automations are not enabled"))?;
            apply_action_to_automation(automations, &action);
            host.broadcast_action(channel, action);
            return Ok(());
        }
        let (summary, state) = host
            .sessions
            .get_mut(channel)
            .ok_or_else(|| anyhow!("unknown channel '{channel}'"))?;
        apply_action_to_session(state, &action);
        let new_summary = summary_from_state(channel, state, Some(summary));
        let delta = summary_delta(summary, &new_summary);
        *summary = new_summary;
        host.broadcast_action(channel, action);
        host.broadcast(
            ROOT_RESOURCE_URI,
            "root/sessionSummaryChanged",
            json!({"channel": ROOT_RESOURCE_URI, "session": channel, "changes": delta}),
        );
        Ok(())
    }

    /// Current state of a session.
    pub fn session_state(&self, uri: &str) -> Option<SessionState> {
        lock(&self.state).sessions.get(uri).map(|(_, s)| s.clone())
    }

    /// Abruptly drop every client connection (simulates a network failure).
    pub fn disconnect_all(&self) {
        let mut host = lock(&self.state);
        for (_, connection) in host.connections.drain() {
            if let Some(task) = connection.task {
                task.abort();
            }
        }
    }
}

async fn serve_connection(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<HostState>>,
) -> Result<()> {
    let required = lock(&state).bearer_token.clone();
    // The handshake callback signature is fixed by tungstenite.
    #[allow(clippy::result_large_err)]
    let callback = move |request: &Request,
                         response: Response|
          -> std::result::Result<Response, ErrorResponse> {
        if let Some(token) = &required {
            let expected = format!("Bearer {token}");
            let provided = request
                .headers()
                .get("Authorization")
                .and_then(|v| v.to_str().ok());
            if provided != Some(expected.as_str()) {
                let mut error = ErrorResponse::new(Some("unauthorized".to_string()));
                *error.status_mut() = StatusCode::UNAUTHORIZED;
                return Err(error);
            }
        }
        Ok(response)
    };
    let socket = tokio_tungstenite::accept_hdr_async(stream, callback).await?;
    let (mut sink, mut source) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let id = {
        let mut host = lock(&state);
        let id = host.next_connection;
        host.next_connection += 1;
        host.connections.insert(
            id,
            Connection {
                tx: tx.clone(),
                subscriptions: HashSet::new(),
                task: None,
            },
        );
        id
    };

    let reader_state = state.clone();
    let reader = tokio::spawn(async move {
        while let Some(Ok(message)) = source.next().await {
            let text = match message {
                Message::Text(text) => text,
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(message) = serde_json::from_str::<JsonRpcMessage>(&text) else {
                continue;
            };
            match message {
                JsonRpcMessage::Request(request) => {
                    let response = handle_request(&reader_state, id, request);
                    if let Ok(text) = serde_json::to_string(&response) {
                        let _ = tx.send(Message::Text(text));
                    }
                }
                JsonRpcMessage::Notification(n) if n.method == "unsubscribe" => {
                    if let Some(channel) = n
                        .params
                        .as_ref()
                        .and_then(|p| p.get("channel"))
                        .and_then(Value::as_str)
                    {
                        if let Some(c) = lock(&reader_state).connections.get_mut(&id) {
                            c.subscriptions.remove(channel);
                        }
                    }
                }
                _ => {}
            }
        }
        lock(&reader_state).connections.remove(&id);
    });

    let writer = async move {
        while let Some(message) = rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    };
    let combined = tokio::spawn(async move {
        writer.await;
    });
    {
        let mut host = lock(&state);
        match host.connections.get_mut(&id) {
            Some(c) => c.task = Some(reader),
            None => reader.abort(),
        }
    }
    let _ = combined.await;
    Ok(())
}

fn handle_request(
    state: &Mutex<HostState>,
    connection: u64,
    request: JsonRpcRequest,
) -> JsonRpcMessage {
    let params = request.params.clone().unwrap_or(Value::Null);
    let result = handle_method(state, connection, &request.method, params);
    match result {
        Ok(result) => JsonRpcMessage::SuccessResponse(JsonRpcSuccessResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request.id,
            result,
        }),
        Err((code, message)) => JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: JsonRpcVersion::V2,
            id: request.id,
            error: JsonRpcError {
                code,
                message,
                data: None,
            },
        }),
    }
}

type MethodResult = std::result::Result<Value, (i32, String)>;

fn invalid(e: impl std::fmt::Display) -> (i32, String) {
    (-32602, e.to_string())
}

fn handle_method(
    state: &Mutex<HostState>,
    connection: u64,
    method: &str,
    params: Value,
) -> MethodResult {
    let mut host = lock(state);
    match method {
        "initialize" => {
            let params: InitializeParams = serde_json::from_value(params).map_err(invalid)?;
            let version = params
                .protocol_versions
                .first()
                .cloned()
                .ok_or_else(|| (-32005, "no protocol versions offered".to_string()))?;
            let channels = params.initial_subscriptions.unwrap_or_default();
            let snapshots: Vec<Snapshot> =
                channels.iter().filter_map(|c| host.snapshot(c)).collect();
            if let Some(c) = host.connections.get_mut(&connection) {
                c.subscriptions.extend(channels);
            }
            let mut result = json!({
                "protocolVersion": version,
                "serverSeq": host.seq,
                "serverInfo": {"name": "drasi-mock-ahp-host", "version": env!("CARGO_PKG_VERSION")},
                "snapshots": snapshots,
            });
            if host.automations.is_some() {
                result["automations"] = json!({});
            }
            Ok(result)
        }
        "reconnect" => {
            let params: ReconnectParams = serde_json::from_value(params).map_err(invalid)?;
            let snapshots: Vec<Snapshot> = params
                .subscriptions
                .iter()
                .filter_map(|c| host.snapshot(c))
                .collect();
            if let Some(c) = host.connections.get_mut(&connection) {
                c.subscriptions.extend(params.subscriptions);
            }
            Ok(json!({"type": "snapshot", "snapshots": snapshots}))
        }
        "subscribe" => {
            let params: SubscribeParams = serde_json::from_value(params).map_err(invalid)?;
            let snapshot = host
                .snapshot(&params.channel)
                .ok_or_else(|| (-32002, format!("unknown channel '{}'", params.channel)))?;
            if let Some(c) = host.connections.get_mut(&connection) {
                c.subscriptions.insert(params.channel);
            }
            Ok(json!({"snapshot": snapshot}))
        }
        "listSessions" => {
            let params: ListSessionsParams = serde_json::from_value(params).map_err(invalid)?;
            let start: usize = params
                .cursor
                .as_deref()
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let limit = params.limit.map_or(usize::MAX, |l| l.max(1) as usize);
            let items: Vec<&SessionSummary> = host
                .sessions
                .values()
                .map(|(summary, _)| summary)
                .skip(start)
                .take(limit)
                .collect();
            let next = start + items.len();
            let mut result = json!({"items": items});
            if next < host.sessions.len() {
                result["nextCursor"] = json!(next.to_string());
            }
            Ok(result)
        }
        "authenticate" => Ok(json!({})),
        "ping" => Ok(Value::Null),
        other => Err((
            -32601,
            format!("method '{other}' not supported by mock host"),
        )),
    }
}
