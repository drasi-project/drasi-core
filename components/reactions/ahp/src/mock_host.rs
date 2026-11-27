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

//! In-process mock AHP host for tests, demos, and examples.
//!
//! Enabled with the `mock-host` feature. It speaks the subset of the Agent
//! Host Protocol the reaction uses over a local WebSocket: `initialize`,
//! `authenticate`, `subscribe`/`unsubscribe`, `createSession`, `createChat`,
//! `disposeSession`, `runAutomation`, and `dispatchAction` with echoes. A
//! simulated agent completes each turn after a configurable delay and then
//! consumes queued follow-ups. **Not for production use.**

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Observable event emitted by the mock host.
#[derive(Debug, Clone, PartialEq)]
pub enum MockHostEvent {
    SessionCreated {
        session: String,
    },
    SessionTitled {
        session: String,
        title: String,
    },
    TurnStarted {
        chat: String,
        turn_id: String,
        text: String,
    },
    TurnCompleted {
        chat: String,
        turn_id: String,
        response: String,
    },
    TurnCancelled {
        chat: String,
        turn_id: String,
    },
    FollowUpQueued {
        chat: String,
        id: String,
        kind: String,
        text: String,
    },
    SessionArchived {
        session: String,
    },
    SessionDisposed {
        session: String,
    },
    AutomationRunStarted {
        automation: String,
        run: String,
        request_id: String,
    },
    AutomationRunCancelled {
        run: String,
    },
}

/// Outcome of a mock turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockTurnState {
    Active,
    Completed,
    Cancelled,
}

/// A turn recorded by the mock host.
#[derive(Debug, Clone)]
pub struct MockTurn {
    pub id: String,
    pub text: String,
    pub meta: Value,
    pub state: MockTurnState,
    pub response: Option<String>,
}

/// A session recorded by the mock host.
#[derive(Debug, Clone)]
pub struct MockSession {
    pub uri: String,
    pub chat: String,
    pub provider: Option<String>,
    pub title: Option<String>,
    pub archived: bool,
    pub disposed: bool,
    pub turns: Vec<MockTurn>,
    pub pending: Vec<(String, String, String)>,
}

/// An automation run recorded by the mock host.
#[derive(Debug, Clone)]
pub struct MockRun {
    pub uri: String,
    pub automation: String,
    pub request_id: String,
    pub meta: Value,
    pub cancelled: bool,
}

type Responder = Arc<dyn Fn(&str) -> String + Send + Sync>;

struct Conn {
    tx: mpsc::UnboundedSender<Value>,
    subs: HashSet<String>,
    authenticated: bool,
    client_id: String,
}

#[derive(Default)]
struct ChatState {
    session: String,
    active: Option<usize>,
    queued: Vec<(String, String, String, Value)>,
}

#[derive(Default)]
struct State {
    conns: HashMap<u64, Conn>,
    sessions: HashMap<String, MockSession>,
    session_order: Vec<String>,
    chats: HashMap<String, ChatState>,
    ready: HashSet<String>,
    runs: HashMap<String, MockRun>,
    run_order: Vec<String>,
}

struct Inner {
    state: Mutex<State>,
    seq: AtomicU64,
    turn_delay: Duration,
    ready_delay: Duration,
    auth: Option<(String, String)>,
    responder: Responder,
    events: broadcast::Sender<MockHostEvent>,
}

/// Builder for [`MockAgentHost`].
pub struct MockAgentHostBuilder {
    turn_delay: Duration,
    ready_delay: Duration,
    auth: Option<(String, String)>,
    responder: Responder,
}

impl Default for MockAgentHostBuilder {
    fn default() -> Self {
        Self {
            turn_delay: Duration::from_millis(100),
            ready_delay: Duration::from_millis(10),
            auth: None,
            responder: Arc::new(|text: &str| {
                let first = text.lines().next().unwrap_or_default();
                format!("On it: {first}")
            }),
        }
    }
}

impl MockAgentHostBuilder {
    /// How long the simulated agent takes to finish a turn.
    pub fn with_turn_delay(mut self, delay: Duration) -> Self {
        self.turn_delay = delay;
        self
    }

    /// Require `authenticate` with `resource`/`token` before creating work.
    pub fn with_required_token(mut self, resource: &str, token: &str) -> Self {
        self.auth = Some((resource.to_string(), token.to_string()));
        self
    }

    /// Produce the simulated agent's reply from a turn's message text.
    pub fn with_responder(mut self, f: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        self.responder = Arc::new(f);
        self
    }

    /// Bind to an ephemeral local port and start serving.
    pub async fn start(self) -> anyhow::Result<MockAgentHost> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let (events, _) = broadcast::channel(1024);
        let inner = Arc::new(Inner {
            state: Mutex::new(State::default()),
            seq: AtomicU64::new(1),
            turn_delay: self.turn_delay,
            ready_delay: self.ready_delay,
            auth: self.auth,
            responder: self.responder,
            events,
        });
        let accept_inner = inner.clone();
        let task = tokio::spawn(async move {
            let next_id = AtomicU64::new(1);
            while let Ok((stream, _)) = listener.accept().await {
                let conn_id = next_id.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(serve(accept_inner.clone(), conn_id, stream));
            }
        });
        Ok(MockAgentHost {
            url: format!("ws://{addr}"),
            inner,
            task,
        })
    }
}

/// A running mock AHP host. Stops serving when dropped.
pub struct MockAgentHost {
    url: String,
    inner: Arc<Inner>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockAgentHost {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockAgentHost {
    /// Builder with default settings.
    pub fn builder() -> MockAgentHostBuilder {
        MockAgentHostBuilder::default()
    }

    /// Start a mock host with default settings.
    pub async fn start() -> anyhow::Result<Self> {
        Self::builder().start().await
    }

    /// WebSocket URL to connect to (`ws://127.0.0.1:<port>`).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Subscribe to host events.
    pub fn events(&self) -> broadcast::Receiver<MockHostEvent> {
        self.inner.events.subscribe()
    }

    /// Sessions in creation order.
    pub fn sessions(&self) -> Vec<MockSession> {
        let state = lock(&self.inner.state);
        state
            .session_order
            .iter()
            .filter_map(|uri| state.sessions.get(uri).cloned())
            .collect()
    }

    /// Automation runs in creation order.
    pub fn runs(&self) -> Vec<MockRun> {
        let state = lock(&self.inner.state);
        state
            .run_order
            .iter()
            .filter_map(|uri| state.runs.get(uri).cloned())
            .collect()
    }

    /// Wait until `predicate` holds for the current sessions.
    pub async fn wait_for_sessions(
        &self,
        timeout: Duration,
        predicate: impl Fn(&[MockSession]) -> bool,
    ) -> anyhow::Result<Vec<MockSession>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let sessions = self.sessions();
            if predicate(&sessions) {
                return Ok(sessions);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for mock host sessions: {sessions:#?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait until `predicate` holds for the current automation runs.
    pub async fn wait_for_runs(
        &self,
        timeout: Duration,
        predicate: impl Fn(&[MockRun]) -> bool,
    ) -> anyhow::Result<Vec<MockRun>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let runs = self.runs();
            if predicate(&runs) {
                return Ok(runs);
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for mock host runs: {runs:#?}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

async fn serve(inner: Arc<Inner>, conn_id: u64, stream: tokio::net::TcpStream) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = ws.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    lock(&inner.state).conns.insert(
        conn_id,
        Conn {
            tx,
            subs: HashSet::new(),
            authenticated: inner.auth.is_none(),
            client_id: format!("conn-{conn_id}"),
        },
    );
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(WsMessage::Text(msg.to_string())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(frame)) = source.next().await {
        let text = match frame {
            WsMessage::Text(t) => t,
            WsMessage::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            WsMessage::Close(_) => break,
            _ => continue,
        };
        if let Ok(msg) = serde_json::from_str::<Value>(&text) {
            handle(&inner, conn_id, msg);
        }
    }
    lock(&inner.state).conns.remove(&conn_id);
    writer.abort();
}

fn send(state: &State, conn_id: u64, msg: Value) {
    if let Some(conn) = state.conns.get(&conn_id) {
        let _ = conn.tx.send(msg);
    }
}

fn reply(state: &State, conn_id: u64, id: &Value, result: Result<Value, (i32, String)>) {
    let msg = match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        }
    };
    send(state, conn_id, msg);
}

fn handle(inner: &Arc<Inner>, conn_id: u64, msg: Value) {
    let method = msg
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let channel = params
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let Some(id) = msg.get("id").cloned() else {
        match method {
            "dispatchAction" => dispatch(inner, conn_id, &channel, &params),
            "unsubscribe" => {
                if let Some(conn) = lock(&inner.state).conns.get_mut(&conn_id) {
                    conn.subs.remove(&channel);
                }
            }
            _ => {}
        }
        return;
    };

    let mut state = lock(&inner.state);
    let authed = state.conns.get(&conn_id).is_some_and(|c| c.authenticated);
    let result: Result<Value, (i32, String)> = match method {
        "initialize" => {
            if let (Some(conn), Some(client_id)) = (
                state.conns.get_mut(&conn_id),
                params.get("clientId").and_then(Value::as_str),
            ) {
                conn.client_id = client_id.to_string();
            }
            Ok(json!({
            "protocolVersion": params
                .pointer("/protocolVersions/0")
                .cloned()
                .unwrap_or(json!("1.0.0")),
            "serverSeq": inner.seq.load(Ordering::Relaxed),
            "snapshots": [],
            }))
        }
        "authenticate" => {
            let ok = match &inner.auth {
                Some((resource, token)) => {
                    params.get("resource").and_then(Value::as_str) == Some(resource)
                        && params.get("token").and_then(Value::as_str) == Some(token)
                }
                None => true,
            };
            if ok {
                if let Some(conn) = state.conns.get_mut(&conn_id) {
                    conn.authenticated = true;
                }
                Ok(json!({}))
            } else {
                Err((-32007, "invalid token".to_string()))
            }
        }
        "createSession" | "runAutomation" if !authed => {
            Err((-32007, "authentication required".to_string()))
        }
        "createSession" => {
            if state.sessions.contains_key(&channel) {
                Err((-32003, format!("session '{channel}' already exists")))
            } else {
                let chat = format!(
                    "ahp-chat:/{}-default",
                    channel.trim_start_matches("ahp-session:/")
                );
                state.sessions.insert(
                    channel.clone(),
                    MockSession {
                        uri: channel.clone(),
                        chat: chat.clone(),
                        provider: params
                            .get("provider")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        title: None,
                        archived: false,
                        disposed: false,
                        turns: Vec::new(),
                        pending: Vec::new(),
                    },
                );
                state.session_order.push(channel.clone());
                state.chats.insert(
                    chat.clone(),
                    ChatState {
                        session: channel.clone(),
                        ..ChatState::default()
                    },
                );
                let _ = inner.events.send(MockHostEvent::SessionCreated {
                    session: channel.clone(),
                });
                let inner = inner.clone();
                let session = channel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(inner.ready_delay).await;
                    let mut state = lock(&inner.state);
                    state.ready.insert(session.clone());
                    broadcast_action(
                        &inner,
                        &state,
                        &session,
                        json!({"type": "session/defaultChatChanged", "defaultChat": chat}),
                        None,
                    );
                    broadcast_action(
                        &inner,
                        &state,
                        &session,
                        json!({"type": "session/ready"}),
                        None,
                    );
                    drop(state);
                });
                Ok(Value::Null)
            }
        }
        "createChat" => Ok(Value::Null),
        "disposeSession" => match state.sessions.get_mut(&channel) {
            Some(session) if !session.disposed => {
                session.disposed = true;
                let chat = session.chat.clone();
                state.chats.remove(&chat);
                let _ = inner.events.send(MockHostEvent::SessionDisposed {
                    session: channel.clone(),
                });
                Ok(Value::Null)
            }
            _ => Err((-32001, format!("session '{channel}' not found"))),
        },
        "runAutomation" => {
            let automation = params
                .get("automation")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let request_id = params
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let existing = state
                .runs
                .values()
                .find(|r| r.automation == automation && r.request_id == request_id)
                .map(|r| r.uri.clone());
            let uri = match existing {
                Some(uri) => uri,
                None => {
                    let uri = format!("ahp-automation-run:/run-{}", state.runs.len() + 1);
                    state.runs.insert(
                        uri.clone(),
                        MockRun {
                            uri: uri.clone(),
                            automation: automation.clone(),
                            request_id: request_id.clone(),
                            meta: params.get("_meta").cloned().unwrap_or(Value::Null),
                            cancelled: false,
                        },
                    );
                    state.run_order.push(uri.clone());
                    let _ = inner.events.send(MockHostEvent::AutomationRunStarted {
                        automation,
                        run: uri.clone(),
                        request_id,
                    });
                    uri
                }
            };
            Ok(json!({ "resource": uri }))
        }
        "subscribe" => match snapshot(&state, &channel) {
            Some(snap) => {
                if let Some(conn) = state.conns.get_mut(&conn_id) {
                    conn.subs.insert(channel.clone());
                }
                Ok(
                    json!({ "snapshot": { "resource": channel, "state": snap, "fromSeq": inner.seq.load(Ordering::Relaxed) } }),
                )
            }
            None => Err((-32001, format!("channel '{channel}' not found"))),
        },
        "ping" => Ok(json!({})),
        other => Err((
            -32601,
            format!("method '{other}' not supported by mock host"),
        )),
    };
    reply(&state, conn_id, &id, result);
}

fn snapshot(state: &State, channel: &str) -> Option<Value> {
    if channel == "ahp-root://" || channel == "ahp-automations://" {
        return Some(json!({}));
    }
    if let Some(session) = state.sessions.get(channel).filter(|s| !s.disposed) {
        let ready = state.ready.contains(channel);
        let mut snap = json!({
            "lifecycle": if ready { "ready" } else { "creating" },
            "isArchived": session.archived,
        });
        if ready {
            snap["defaultChat"] = json!(session.chat);
            snap["chats"] = json!([{ "resource": session.chat }]);
        }
        if let Some(title) = &session.title {
            snap["title"] = json!(title);
        }
        return Some(snap);
    }
    if let Some(chat) = state.chats.get(channel) {
        let session = state.sessions.get(&chat.session)?;
        let turns: Vec<Value> = session
            .turns
            .iter()
            .filter(|t| t.state != MockTurnState::Active)
            .map(|t| json!({ "id": t.id }))
            .collect();
        let mut snap = json!({
            "turns": turns,
            "queuedMessages": chat.queued.iter().map(|(id, kind, _, _)| json!({"id": id, "kind": kind})).collect::<Vec<_>>(),
        });
        if let Some(index) = chat.active {
            snap["activeTurn"] = json!({ "id": session.turns[index].id });
        }
        return Some(snap);
    }
    state
        .runs
        .get(channel)
        .map(|run| json!({ "lifecycle": if run.cancelled { "cancelled" } else { "running" } }))
}

fn broadcast_action(
    inner: &Inner,
    state: &State,
    channel: &str,
    action: Value,
    origin: Option<Value>,
) {
    let seq = inner.seq.fetch_add(1, Ordering::Relaxed);
    let mut params = json!({ "channel": channel, "action": action, "serverSeq": seq });
    if let Some(origin) = origin {
        params["origin"] = origin;
    }
    let msg = json!({ "jsonrpc": "2.0", "method": "action", "params": params });
    for conn in state.conns.values() {
        if conn.subs.contains(channel) {
            let _ = conn.tx.send(msg.clone());
        }
    }
}

fn reject(
    inner: &Inner,
    state: &State,
    conn_id: u64,
    channel: &str,
    action: Value,
    origin: Value,
    reason: &str,
) {
    let seq = inner.seq.fetch_add(1, Ordering::Relaxed);
    send(
        state,
        conn_id,
        json!({ "jsonrpc": "2.0", "method": "action", "params": {
            "channel": channel, "action": action, "serverSeq": seq,
            "origin": origin, "rejectionReason": reason,
        }}),
    );
}

fn dispatch(inner: &Arc<Inner>, conn_id: u64, channel: &str, params: &Value) {
    let action = params.get("action").cloned().unwrap_or(Value::Null);
    let kind = action
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut state = lock(&inner.state);
    let client_id = state
        .conns
        .get(&conn_id)
        .map(|c| c.client_id.clone())
        .unwrap_or_default();
    let origin = json!({
        "clientId": client_id,
        "clientSeq": params.get("clientSeq").cloned().unwrap_or(json!(0)),
    });
    match kind {
        "chat/turnStarted" => {
            let Some(chat) = state.chats.get(channel) else {
                return;
            };
            if chat.active.is_some() {
                reject(
                    inner,
                    &state,
                    conn_id,
                    channel,
                    action,
                    origin,
                    "a turn is already active",
                );
                return;
            }
            let turn_id = action
                .get("turnId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let text = action
                .pointer("/message/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let meta = action
                .pointer("/message/_meta")
                .cloned()
                .unwrap_or(Value::Null);
            broadcast_action(inner, &state, channel, action, Some(origin));
            start_turn(inner, &mut state, channel, turn_id, text, meta);
        }
        "chat/pendingMessageSet" => {
            if !state.chats.contains_key(channel) {
                return;
            }
            let id = action
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let pending_kind = action
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("queued")
                .to_string();
            let text = action
                .pointer("/message/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let meta = action
                .pointer("/message/_meta")
                .cloned()
                .unwrap_or(Value::Null);
            broadcast_action(inner, &state, channel, action, Some(origin));
            let session_uri = state
                .chats
                .get(channel)
                .map(|c| c.session.clone())
                .unwrap_or_default();
            if let Some(session) = state.sessions.get_mut(&session_uri) {
                session
                    .pending
                    .push((id.clone(), pending_kind.clone(), text.clone()));
            }
            let _ = inner.events.send(MockHostEvent::FollowUpQueued {
                chat: channel.to_string(),
                id: id.clone(),
                kind: pending_kind.clone(),
                text: text.clone(),
            });
            if let Some(chat) = state.chats.get_mut(channel) {
                if let Some(existing) = chat.queued.iter_mut().find(|q| q.0 == id) {
                    *existing = (id, pending_kind, text, meta);
                } else {
                    chat.queued.push((id, pending_kind, text, meta));
                }
            }
            consume_queued(inner, &mut state, channel);
        }
        "chat/turnCancelled" => {
            let Some(chat) = state.chats.get(channel) else {
                return;
            };
            let Some(index) = chat.active else {
                reject(
                    inner,
                    &state,
                    conn_id,
                    channel,
                    action,
                    origin,
                    "no active turn",
                );
                return;
            };
            let session_uri = chat.session.clone();
            broadcast_action(inner, &state, channel, action, Some(origin));
            if let Some(chat) = state.chats.get_mut(channel) {
                chat.active = None;
            }
            if let Some(turn) = state
                .sessions
                .get_mut(&session_uri)
                .and_then(|s| s.turns.get_mut(index))
            {
                turn.state = MockTurnState::Cancelled;
                let _ = inner.events.send(MockHostEvent::TurnCancelled {
                    chat: channel.to_string(),
                    turn_id: turn.id.clone(),
                });
            }
            consume_queued(inner, &mut state, channel);
        }
        "session/titleChanged" => {
            let title = action
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(session) = state.sessions.get_mut(channel) else {
                return;
            };
            session.title = Some(title.clone());
            let _ = inner.events.send(MockHostEvent::SessionTitled {
                session: channel.to_string(),
                title,
            });
            broadcast_action(inner, &state, channel, action, Some(origin));
        }
        "session/isArchivedChanged" => {
            let archived = action
                .get("isArchived")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let Some(session) = state.sessions.get_mut(channel) else {
                return;
            };
            session.archived = archived;
            if archived {
                let _ = inner.events.send(MockHostEvent::SessionArchived {
                    session: channel.to_string(),
                });
            }
            broadcast_action(inner, &state, channel, action, Some(origin));
        }
        "automationRun/cancelRequested" => {
            let Some(run) = state.runs.get_mut(channel) else {
                return;
            };
            if run.cancelled {
                reject(
                    inner,
                    &state,
                    conn_id,
                    channel,
                    action,
                    origin,
                    "run is terminal",
                );
                return;
            }
            run.cancelled = true;
            let _ = inner.events.send(MockHostEvent::AutomationRunCancelled {
                run: channel.to_string(),
            });
            broadcast_action(inner, &state, channel, action, Some(origin));
        }
        _ => broadcast_action(inner, &state, channel, action, Some(origin)),
    }
}

fn start_turn(
    inner: &Arc<Inner>,
    state: &mut State,
    chat_uri: &str,
    turn_id: String,
    text: String,
    meta: Value,
) {
    let Some(session_uri) = state.chats.get(chat_uri).map(|c| c.session.clone()) else {
        return;
    };
    let Some(session) = state.sessions.get_mut(&session_uri) else {
        return;
    };
    session.turns.push(MockTurn {
        id: turn_id.clone(),
        text: text.clone(),
        meta,
        state: MockTurnState::Active,
        response: None,
    });
    let index = session.turns.len() - 1;
    if let Some(chat) = state.chats.get_mut(chat_uri) {
        chat.active = Some(index);
    }
    let _ = inner.events.send(MockHostEvent::TurnStarted {
        chat: chat_uri.to_string(),
        turn_id: turn_id.clone(),
        text,
    });

    let inner = inner.clone();
    let chat_uri = chat_uri.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(inner.turn_delay).await;
        let mut state = lock(&inner.state);
        if state.chats.get(&chat_uri).and_then(|c| c.active) != Some(index) {
            return;
        }
        let Some(turn) = state
            .sessions
            .get_mut(&session_uri)
            .and_then(|s| s.turns.get_mut(index))
        else {
            return;
        };
        let response = (inner.responder)(&turn.text);
        turn.state = MockTurnState::Completed;
        turn.response = Some(response.clone());
        if let Some(chat) = state.chats.get_mut(&chat_uri) {
            chat.active = None;
        }
        let _ = inner.events.send(MockHostEvent::TurnCompleted {
            chat: chat_uri.clone(),
            turn_id: turn_id.clone(),
            response,
        });
        broadcast_action(
            &inner,
            &state,
            &chat_uri,
            json!({"type": "chat/turnComplete", "turnId": turn_id, "duration": inner.turn_delay.as_millis() as u64}),
            None,
        );
        consume_queued(&inner, &mut state, &chat_uri);
    });
}

/// Start a turn from the first queued message when the chat is idle;
/// steering messages are folded into the active turn.
fn consume_queued(inner: &Arc<Inner>, state: &mut State, chat_uri: &str) {
    let Some(chat) = state.chats.get_mut(chat_uri) else {
        return;
    };
    if chat.active.is_some() {
        chat.queued.retain(|(_, kind, _, _)| kind != "steering");
        return;
    }
    let Some(pos) = chat
        .queued
        .iter()
        .position(|(_, kind, _, _)| kind == "queued")
    else {
        return;
    };
    let (id, _, text, meta) = chat.queued.remove(pos);
    broadcast_action(
        inner,
        state,
        chat_uri,
        json!({"type": "chat/pendingMessageRemoved", "kind": "queued", "id": id}),
        None,
    );
    start_turn(inner, state, chat_uri, format!("turn-{id}"), text, meta);
}
