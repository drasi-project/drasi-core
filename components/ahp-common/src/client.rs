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

//! Minimal AHP JSON-RPC 2.0 client over WebSocket.
//!
//! The upstream `ahp::Client` fans notifications out through bounded
//! broadcast channels that silently skip messages when a consumer lags. A
//! Drasi source cannot tolerate silent gaps (the mirrored state would drift),
//! so this client delivers every server notification, in wire order, through
//! an unbounded queue and leaves backpressure to the TCP connection. It reuses
//! the generated `ahp-types` wire types and the workspace `tokio-tungstenite`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ahp_types::actions::ActionEnvelope;
use ahp_types::messages::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcVersion,
};
use ahp_types::notifications::{
    AuthRequiredParams, SessionAddedParams, SessionRemovedParams, SessionSummaryChangedParams,
};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use log::{debug, warn};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::sync::FatalAhpError;

/// JSON-RPC error code for "method not found".
const METHOD_NOT_FOUND: i32 = -32601;

/// A server-pushed event, in wire order.
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// `action` notification.
    Action(ActionEnvelope),
    /// `root/sessionAdded` notification.
    SessionAdded(SessionAddedParams),
    /// `root/sessionRemoved` notification.
    SessionRemoved(SessionRemovedParams),
    /// `root/sessionSummaryChanged` notification.
    SessionSummaryChanged(SessionSummaryChangedParams),
    /// `auth/required` notification.
    AuthRequired(AuthRequiredParams),
    /// The connection closed; no further events will arrive.
    Closed(String),
}

/// Error returned by [`AhpConnection::request`].
#[derive(Debug)]
pub enum RequestError {
    /// The server answered with a JSON-RPC error.
    Rpc(JsonRpcError),
    /// The request did not complete within the configured timeout.
    Timeout,
    /// The connection closed before a response arrived.
    Closed,
    /// The request or response could not be (de)serialized.
    Codec(String),
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(e) => write!(f, "AHP error {}: {}", e.code, e.message),
            Self::Timeout => write!(f, "AHP request timed out"),
            Self::Closed => write!(f, "AHP connection closed"),
            Self::Codec(e) => write!(f, "AHP codec error: {e}"),
        }
    }
}

impl std::error::Error for RequestError {}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, JsonRpcError>>>>>;

/// An open AHP connection. Dropping it closes the socket.
pub struct AhpConnection {
    outbound: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next_id: AtomicU64,
    request_timeout: Duration,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for AhpConnection {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Install the ring crypto provider for rustls (idempotent).
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl AhpConnection {
    /// Open a WebSocket connection to an AHP host.
    ///
    /// `bearer_token`, when set, is sent as an `Authorization: Bearer` header
    /// on the upgrade request. Returns the connection and the ordered stream
    /// of server events.
    pub async fn connect(
        url: &str,
        bearer_token: Option<&str>,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<(Self, mpsc::UnboundedReceiver<ServerEvent>)> {
        ensure_crypto_provider();
        let mut request = url
            .into_client_request()
            .with_context(|| format!("invalid AHP URL '{url}'"))?;
        if let Some(token) = bearer_token {
            let value = HeaderValue::from_str(&format!("Bearer {token}"))
                .context("invalid bearer token")?;
            request.headers_mut().insert("Authorization", value);
        }
        let (socket, _) =
            tokio::time::timeout(connect_timeout, tokio_tungstenite::connect_async(request))
                .await
                .map_err(|_| anyhow!("timed out connecting to AHP host at '{url}'"))?
                .map_err(|e| match &e {
                    tokio_tungstenite::tungstenite::Error::Http(response)
                        if response.status().as_u16() == 401
                            || response.status().as_u16() == 403 =>
                    {
                        anyhow::Error::new(FatalAhpError(format!(
                            "AHP host at '{url}' rejected the connection ({}); check bearerToken",
                            response.status()
                        )))
                    }
                    _ => anyhow::Error::new(e)
                        .context(format!("failed to connect to AHP host at '{url}'")),
                })?;

        let (mut sink, mut stream) = socket.split();
        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Message>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<ServerEvent>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let writer = tokio::spawn(async move {
            while let Some(message) = outbound_rx.recv().await {
                let close = matches!(message, Message::Close(_));
                if let Err(e) = sink.send(message).await {
                    debug!("AHP send failed: {e}");
                    break;
                }
                if close {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let reader_pending = pending.clone();
        let reader_outbound = outbound_tx.clone();
        let reader = tokio::spawn(async move {
            let reason = loop {
                let message = match stream.next().await {
                    Some(Ok(message)) => message,
                    Some(Err(e)) => break format!("WebSocket error: {e}"),
                    None => break "connection closed".to_string(),
                };
                let text = match message {
                    Message::Text(text) => text,
                    Message::Binary(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => text,
                        Err(_) => {
                            warn!("Ignoring non-UTF-8 AHP frame");
                            continue;
                        }
                    },
                    Message::Close(frame) => {
                        break frame.map_or_else(
                            || "connection closed by host".to_string(),
                            |f| format!("connection closed by host: {}", f.reason),
                        )
                    }
                    _ => continue,
                };
                match serde_json::from_str::<JsonRpcMessage>(&text) {
                    Ok(message) => {
                        handle_inbound(message, &reader_pending, &reader_outbound, &event_tx)
                    }
                    Err(e) => warn!("Ignoring malformed AHP message: {e}"),
                }
            };
            fail_pending(&reader_pending);
            let _ = event_tx.send(ServerEvent::Closed(reason));
        });

        Ok((
            Self {
                outbound: outbound_tx,
                pending,
                next_id: AtomicU64::new(1),
                request_timeout,
                tasks: vec![reader, writer],
            },
            event_rx,
        ))
    }

    /// Send a JSON-RPC request and await its typed result.
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> std::result::Result<R, RequestError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let params =
            serde_json::to_value(params).map_err(|e| RequestError::Codec(e.to_string()))?;
        let message = JsonRpcMessage::Request(JsonRpcRequest {
            jsonrpc: JsonRpcVersion::V2,
            id,
            method: method.to_string(),
            params: (!params.is_null()).then_some(params),
        });
        let text =
            serde_json::to_string(&message).map_err(|e| RequestError::Codec(e.to_string()))?;

        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if self.outbound.send(Message::Text(text)).is_err() {
            lock(&self.pending).remove(&id);
            return Err(RequestError::Closed);
        }
        let result = match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => return Err(RequestError::Closed),
            Err(_) => {
                lock(&self.pending).remove(&id);
                return Err(RequestError::Timeout);
            }
        };
        let value = result.map_err(RequestError::Rpc)?;
        serde_json::from_value(value).map_err(|e| RequestError::Codec(e.to_string()))
    }

    /// Send a JSON-RPC notification.
    pub fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<()> {
        let params = serde_json::to_value(params)?;
        let message = JsonRpcMessage::Notification(JsonRpcNotification {
            jsonrpc: JsonRpcVersion::V2,
            method: method.to_string(),
            params: (!params.is_null()).then_some(params),
        });
        let text = serde_json::to_string(&message)?;
        if self.outbound.send(Message::Text(text)).is_err() {
            bail!("AHP connection closed");
        }
        Ok(())
    }

    /// Close the connection gracefully.
    pub fn close(&self) {
        let _ = self.outbound.send(Message::Close(None));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fail_pending(pending: &Pending) {
    // Dropping the senders resolves waiters with `RequestError::Closed`.
    lock(pending).clear();
}

fn handle_inbound(
    message: JsonRpcMessage,
    pending: &Pending,
    outbound: &mpsc::UnboundedSender<Message>,
    events: &mpsc::UnboundedSender<ServerEvent>,
) {
    match message {
        JsonRpcMessage::SuccessResponse(r) => {
            if let Some(tx) = lock(pending).remove(&r.id) {
                let _ = tx.send(Ok(r.result));
            }
        }
        JsonRpcMessage::ErrorResponse(r) => {
            if let Some(tx) = lock(pending).remove(&r.id) {
                let _ = tx.send(Err(r.error));
            }
        }
        JsonRpcMessage::Notification(n) => {
            let params = n.params.unwrap_or(Value::Null);
            let event = match n.method.as_str() {
                "action" => decode(params).map(ServerEvent::Action),
                "root/sessionAdded" => decode(params).map(ServerEvent::SessionAdded),
                "root/sessionRemoved" => decode(params).map(ServerEvent::SessionRemoved),
                "root/sessionSummaryChanged" => {
                    decode(params).map(ServerEvent::SessionSummaryChanged)
                }
                "auth/required" => decode(params).map(ServerEvent::AuthRequired),
                other => {
                    debug!("Ignoring AHP notification '{other}'");
                    return;
                }
            };
            match event {
                Ok(event) => {
                    let _ = events.send(event);
                }
                Err(e) => warn!("Ignoring undecodable AHP '{}' notification: {e}", n.method),
            }
        }
        JsonRpcMessage::Request(r) => {
            // This client exposes no client-side resources.
            let response = JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
                jsonrpc: JsonRpcVersion::V2,
                id: r.id,
                error: JsonRpcError {
                    code: METHOD_NOT_FOUND,
                    message: format!("method '{}' not supported by Drasi", r.method),
                    data: None,
                },
            });
            if let Ok(text) = serde_json::to_string(&response) {
                let _ = outbound.send(Message::Text(text));
            }
        }
    }
}

fn decode<T: DeserializeOwned>(value: Value) -> serde_json::Result<T> {
    serde_json::from_value(value)
}
