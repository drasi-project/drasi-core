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

use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::{net::TcpStream, sync::oneshot, time::timeout};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        error::ProtocolError,
        http::header::{HeaderName, HeaderValue},
        protocol::{frame::coding::CloseCode, WebSocketConfig},
        Error as WebSocketError, Message,
    },
    MaybeTlsStream, WebSocketStream,
};
use tracing::warn;

use drasi_lib::SourceBase;

use crate::{
    config::WebSocketSourceConfig,
    mapping::{FrameError, FrameMapper},
};

pub(crate) type WebSocketConnection = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub(crate) const SHUTDOWN_DISPATCH_GRACE: Duration = Duration::from_millis(250);

pub(crate) enum SessionEnd {
    Shutdown,
    Disconnected { clean: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectErrorDisposition {
    Retry,
    Fatal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchResult {
    Continue,
    Shutdown,
}

pub(crate) async fn connect(config: &WebSocketSourceConfig) -> Result<WebSocketConnection> {
    timeout(
        Duration::from_millis(config.connect_timeout_ms),
        connect_inner(config),
    )
    .await
    .context("WebSocket connection setup timed out")?
}

async fn connect_inner(config: &WebSocketSourceConfig) -> Result<WebSocketConnection> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut request = config
        .url
        .as_str()
        .into_client_request()
        .context("failed to build WebSocket upgrade request")?;
    for header in &config.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .with_context(|| format!("invalid header name '{}'", header.name))?;
        let value = HeaderValue::from_bytes(header.value.as_bytes())
            .with_context(|| format!("invalid value for header '{}'", header.name))?;
        request.headers_mut().insert(name, value);
    }

    let websocket_config = WebSocketConfig {
        max_message_size: Some(config.max_message_size_bytes),
        max_frame_size: Some(config.max_message_size_bytes),
        ..Default::default()
    };

    let (mut socket, _) = connect_async_with_config(request, Some(websocket_config), false)
        .await
        .context("WebSocket handshake failed")?;

    for message in &config.initial_messages {
        socket
            .send(Message::Text(message.clone()))
            .await
            .context("failed to send initial WebSocket message")?;
    }
    socket
        .flush()
        .await
        .context("failed to flush initial WebSocket messages")?;

    Ok(socket)
}

pub(crate) fn connect_error_disposition(error: &anyhow::Error) -> ConnectErrorDisposition {
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
    {
        return ConnectErrorDisposition::Retry;
    }

    match error.downcast_ref::<WebSocketError>() {
        Some(WebSocketError::Io(_)) => ConnectErrorDisposition::Retry,
        Some(WebSocketError::Http(response))
            if matches!(
                response.status().as_u16(),
                408 | 425 | 429 | 500 | 502 | 503 | 504
            ) =>
        {
            ConnectErrorDisposition::Retry
        }
        _ => ConnectErrorDisposition::Fatal,
    }
}

pub(crate) fn safe_error_description(error: &anyhow::Error) -> String {
    if error
        .downcast_ref::<tokio::time::error::Elapsed>()
        .is_some()
    {
        return "WebSocket connection setup timed out".to_string();
    }

    if let Some(frame_error) = error.downcast_ref::<FrameError>() {
        return frame_error.to_string();
    }

    match error.downcast_ref::<WebSocketError>() {
        Some(WebSocketError::Io(error)) => {
            format!("WebSocket I/O error ({:?})", error.kind())
        }
        Some(WebSocketError::Tls(_)) => "WebSocket TLS negotiation failed".to_string(),
        Some(WebSocketError::Capacity(_)) => {
            "WebSocket message exceeded a configured capacity limit".to_string()
        }
        Some(WebSocketError::Protocol(_)) => "WebSocket protocol error".to_string(),
        Some(WebSocketError::Utf8) => "WebSocket message contains invalid UTF-8".to_string(),
        Some(WebSocketError::AttackAttempt) => "WebSocket attack pattern detected".to_string(),
        Some(WebSocketError::Url(_)) => "Invalid WebSocket URL".to_string(),
        Some(WebSocketError::Http(response)) => {
            format!("WebSocket handshake returned HTTP {}", response.status())
        }
        Some(WebSocketError::HttpFormat(_)) => "Invalid WebSocket upgrade request".to_string(),
        Some(WebSocketError::ConnectionClosed) => "WebSocket connection closed".to_string(),
        Some(WebSocketError::AlreadyClosed) => {
            "WebSocket connection was already closed".to_string()
        }
        Some(WebSocketError::WriteBufferFull(_)) => "WebSocket write buffer is full".to_string(),
        None => "WebSocket operation failed".to_string(),
    }
}

pub(crate) async fn run_session(
    socket: &mut WebSocketConnection,
    base: &SourceBase,
    mapper: &FrameMapper,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<SessionEnd> {
    loop {
        tokio::select! {
            biased;
            _ = &mut *shutdown_rx => {
                close_for_shutdown(socket).await;
                return Ok(SessionEnd::Shutdown);
            }
            frame = socket.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let changes = match mapper.map_text(&base.id, &text) {
                            Ok(changes) => changes,
                            Err(error) if error.is_recoverable() => {
                                warn!(
                                    "[{}] Skipping malformed WebSocket text message: {error}",
                                    base.id
                                );
                                continue;
                            }
                            Err(error) => return Err(error.into()),
                        };
                        for change in changes {
                            if dispatch_with_shutdown(base, change, shutdown_rx).await?
                                == DispatchResult::Shutdown
                            {
                                close_for_shutdown(socket).await;
                                return Ok(SessionEnd::Shutdown);
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        return Err(FrameError::BinaryMessage.into());
                    }
                    Some(Ok(Message::Ping(_))) => {
                        if let Err(error) = socket.flush().await {
                            if is_fatal_session_error(&error) {
                                return Err(error).context("fatal WebSocket Pong flush error");
                            }
                            return Ok(SessionEnd::Disconnected { clean: false });
                        }
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        let _ = socket.flush().await;
                        let code = frame.as_ref().map(|frame| frame.code);
                        let clean = matches!(
                            code,
                            None | Some(CloseCode::Normal) | Some(CloseCode::Away)
                        );
                        return Ok(SessionEnd::Disconnected { clean });
                    }
                    Some(Err(WebSocketError::ConnectionClosed)) => {
                        return Ok(SessionEnd::Disconnected { clean: true });
                    }
                    Some(Err(error)) => {
                        if is_fatal_session_error(&error) {
                            return Err(error).context("fatal WebSocket receive error");
                        }
                        return Ok(SessionEnd::Disconnected { clean: false });
                    }
                    None => {
                        return Ok(SessionEnd::Disconnected { clean: true });
                    }
                }
            }
        }
    }
}

async fn dispatch_with_shutdown(
    base: &SourceBase,
    change: drasi_core::models::SourceChange,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<DispatchResult> {
    let dispatch = base.dispatch_source_change(change);
    tokio::pin!(dispatch);

    tokio::select! {
        biased;
        _ = &mut *shutdown_rx => {
            let _ = timeout(SHUTDOWN_DISPATCH_GRACE, &mut dispatch).await;
            Ok(DispatchResult::Shutdown)
        }
        result = &mut dispatch => {
            result?;
            Ok(DispatchResult::Continue)
        }
    }
}

async fn close_for_shutdown(socket: &mut WebSocketConnection) {
    let _ = timeout(SHUTDOWN_DISPATCH_GRACE, socket.close(None)).await;
}

fn is_fatal_session_error(error: &WebSocketError) -> bool {
    match error {
        WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => false,
        WebSocketError::Tls(_)
        | WebSocketError::Capacity(_)
        | WebSocketError::Protocol(_)
        | WebSocketError::Utf8
        | WebSocketError::AttackAttempt => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use anyhow::Context as _;
    use drasi_source_mapping::{ElementTemplate, ElementType, OperationType, SourceMapping};
    use futures_util::poll;
    use tokio_tungstenite::tungstenite::error::{CapacityError, ProtocolError, TlsError};
    use tokio_tungstenite::tungstenite::http::{Response, StatusCode};

    use super::*;
    use crate::config::WebSocketSourceConfig;

    #[test]
    fn classifies_tls_capacity_invalid_protocol_utf8_and_attack_errors_as_fatal() {
        let oversized = WebSocketError::Capacity(CapacityError::MessageTooLong {
            size: 2_048,
            max_size: 1_024,
        });
        assert!(is_fatal_session_error(&WebSocketError::Tls(
            TlsError::InvalidDnsName
        )));
        assert!(is_fatal_session_error(&oversized));
        assert!(is_fatal_session_error(&WebSocketError::Protocol(
            ProtocolError::InvalidOpcode(3)
        )));
        assert!(is_fatal_session_error(&WebSocketError::Utf8));
        assert!(is_fatal_session_error(&WebSocketError::AttackAttempt));
    }

    #[test]
    fn classifies_abrupt_eof_as_reconnectable_disconnect() {
        assert!(!is_fatal_session_error(&WebSocketError::Protocol(
            ProtocolError::ResetWithoutClosingHandshake
        )));
    }

    #[test]
    fn classifies_io_and_retryable_http_statuses_for_retry() {
        let refused = WebSocketError::Io(std::io::Error::from(ErrorKind::ConnectionRefused));

        assert_eq!(
            connect_error_disposition(&anyhow::Error::new(refused)),
            ConnectErrorDisposition::Retry
        );
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let error =
                WebSocketError::Http(Response::builder().status(status).body(None).unwrap());
            assert_eq!(
                connect_error_disposition(&anyhow::Error::new(error).context("handshake failed")),
                ConnectErrorDisposition::Retry
            );
        }
    }

    #[test]
    fn classifies_redirects_and_non_retryable_http_statuses_as_fatal() {
        for status in [
            StatusCode::MOVED_PERMANENTLY,
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::NOT_IMPLEMENTED,
            StatusCode::HTTP_VERSION_NOT_SUPPORTED,
        ] {
            let error =
                WebSocketError::Http(Response::builder().status(status).body(None).unwrap());
            assert_eq!(
                connect_error_disposition(&anyhow::Error::new(error).context("handshake failed")),
                ConnectErrorDisposition::Fatal
            );
        }
    }

    #[tokio::test]
    async fn classifies_connection_timeout_as_retryable() {
        let elapsed = timeout(Duration::ZERO, std::future::pending::<()>())
            .await
            .unwrap_err();
        assert_eq!(
            connect_error_disposition(&anyhow::Error::new(elapsed)),
            ConnectErrorDisposition::Retry
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_dispatch_blocked_by_full_source_channel() {
        let base = SourceBase::new(
            drasi_lib::SourceBaseParams::new("source").with_dispatch_buffer_capacity(1),
        )
        .unwrap();
        let _receiver = base.create_streaming_receiver().await.unwrap();

        base.dispatch_source_change(test_change("first"))
            .await
            .unwrap();

        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        let blocked = dispatch_with_shutdown(&base, test_change("second"), &mut shutdown_rx);
        tokio::pin!(blocked);
        assert!(poll!(&mut blocked).is_pending());

        shutdown_tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(1), &mut blocked)
            .await
            .expect("dispatch cancellation must not reach the five-second stop fallback")
            .unwrap();
        assert_eq!(result, DispatchResult::Shutdown);
    }

    fn test_change(id: &str) -> drasi_core::models::SourceChange {
        let config = WebSocketSourceConfig {
            url: "wss://example.com".to_string(),
            mappings: vec![SourceMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Item".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
            ..Default::default()
        };

        FrameMapper::new(&config)
            .map_text("source", &serde_json::json!({"id": id}).to_string())
            .unwrap()
            .pop()
            .unwrap()
    }
}
