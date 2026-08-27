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

use std::{future::Future, pin::Pin, time::Duration};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::timeout,
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{
        client::IntoClientRequest,
        error::ProtocolError,
        http::header::{HeaderName, HeaderValue},
        protocol::{frame::coding::CloseCode, WebSocketConfig},
        Error as WebSocketError, Message,
    },
    Connector, MaybeTlsStream, WebSocketStream,
};
use tracing::warn;

use drasi_lib::SourceBase;

use crate::{
    config::WebSocketSourceConfig,
    mapping::{FrameError, FrameMapper},
};

pub(crate) type WebSocketConnection = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub(crate) const SHUTDOWN_DISPATCH_GRACE: Duration = Duration::from_millis(250);
const SESSION_FRAME_BUFFER_CAPACITY: usize = 16;
// The pending slot holds the text frame read when the channel reaches capacity.
const SESSION_FRAME_CHANNEL_CAPACITY: usize = SESSION_FRAME_BUFFER_CAPACITY - 1;

pub(crate) enum SessionEnd {
    Shutdown,
    Disconnected { clean: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnectErrorDisposition {
    Retry,
    Fatal,
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
    connect_inner_with_connector(config, None).await
}

async fn connect_inner_with_connector(
    config: &WebSocketSourceConfig,
    connector: Option<Connector>,
) -> Result<WebSocketConnection> {
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

    let (mut socket, _) =
        connect_async_tls_with_config(request, Some(websocket_config), false, connector)
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
        Some(WebSocketError::Io(error)) if is_tls_io_error(error) => ConnectErrorDisposition::Fatal,
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
        Some(WebSocketError::Io(error)) if is_tls_io_error(error) => {
            "WebSocket TLS negotiation failed".to_string()
        }
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

// Concurrency invariant shared by run_session, drain_dispatch, and
// finish_session: the channel and pending slot cap queued text frames at 16.
// Reads pause while a text frame waits for its reserved channel permit, which
// preserves frame order without an unbounded reader task. Biased selection
// prioritizes shutdown and dispatch completion. A normal disconnect drains the
// queue, while explicit shutdown gives queued and in-flight work only the
// bounded dispatch grace period.
pub(crate) async fn run_session(
    socket: &mut WebSocketConnection,
    base: &SourceBase,
    mapper: &FrameMapper,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<SessionEnd> {
    let (frame_tx, frame_rx) = mpsc::channel(SESSION_FRAME_CHANNEL_CAPACITY);
    let dispatch = map_and_dispatch_frames(frame_rx, base, mapper);
    tokio::pin!(dispatch);
    let mut pending_text = None;

    loop {
        tokio::select! {
            biased;
            _ = &mut *shutdown_rx => {
                let drain = drain_dispatch(
                    frame_tx,
                    pending_text.take(),
                    dispatch.as_mut(),
                );
                let _ = timeout(SHUTDOWN_DISPATCH_GRACE, drain).await;
                close_for_shutdown(socket).await;
                return Ok(SessionEnd::Shutdown);
            }
            result = dispatch.as_mut() => {
                result?;
                bail!("WebSocket dispatch pipeline stopped unexpectedly");
            }
            permit = frame_tx.clone().reserve_owned(), if pending_text.is_some() => {
                let permit = permit.context("WebSocket dispatch pipeline closed")?;
                permit.send(
                    pending_text
                        .take()
                        .expect("pending WebSocket text frame must exist"),
                );
            }
            frame = socket.next(), if pending_text.is_none() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        match frame_tx.try_send(text) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(text)) => {
                                pending_text = Some(text);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                bail!("WebSocket dispatch pipeline closed unexpectedly");
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        warn!(
                            "[{}] Skipping unsupported binary WebSocket message",
                            base.id
                        );
                    }
                    Some(Ok(Message::Ping(_))) => {
                        if let Err(error) = socket.flush().await {
                            if is_fatal_session_error(&error) {
                                return Err(error).context("fatal WebSocket Pong flush error");
                            }
                            return finish_session(
                                frame_tx,
                                dispatch.as_mut(),
                                socket,
                                shutdown_rx,
                                false,
                            )
                            .await;
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
                        return finish_session(
                            frame_tx,
                            dispatch.as_mut(),
                            socket,
                            shutdown_rx,
                            clean,
                        )
                        .await;
                    }
                    Some(Err(WebSocketError::ConnectionClosed)) => {
                        return finish_session(
                            frame_tx,
                            dispatch.as_mut(),
                            socket,
                            shutdown_rx,
                            true,
                        )
                        .await;
                    }
                    Some(Err(error)) => {
                        if is_fatal_session_error(&error) {
                            return Err(error).context("fatal WebSocket receive error");
                        }
                        return finish_session(
                            frame_tx,
                            dispatch.as_mut(),
                            socket,
                            shutdown_rx,
                            false,
                        )
                        .await;
                    }
                    None => {
                        return finish_session(
                            frame_tx,
                            dispatch.as_mut(),
                            socket,
                            shutdown_rx,
                            true,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

async fn map_and_dispatch_frames(
    mut frame_rx: mpsc::Receiver<String>,
    base: &SourceBase,
    mapper: &FrameMapper,
) -> Result<()> {
    while let Some(text) = frame_rx.recv().await {
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
            base.dispatch_source_change(change).await?;
        }
    }

    Ok(())
}

async fn drain_dispatch<F>(
    frame_tx: mpsc::Sender<String>,
    pending_text: Option<String>,
    mut dispatch: Pin<&mut F>,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    // The pending frame is next in socket order and must enter the queue before
    // the sender is dropped for a normal drain.
    if let Some(text) = pending_text {
        let permit = tokio::select! {
            result = &mut dispatch => {
                result?;
                bail!("WebSocket dispatch pipeline stopped unexpectedly");
            }
            permit = frame_tx.clone().reserve_owned() => {
                permit.context("WebSocket dispatch pipeline closed")?
            }
        };
        permit.send(text);
    }

    drop(frame_tx);
    dispatch.await
}

async fn finish_session<F>(
    frame_tx: mpsc::Sender<String>,
    mut dispatch: Pin<&mut F>,
    socket: &mut WebSocketConnection,
    shutdown_rx: &mut oneshot::Receiver<()>,
    clean: bool,
) -> Result<SessionEnd>
where
    F: Future<Output = Result<()>>,
{
    // Dropping the sender turns a disconnect into an ordered drain; only an
    // explicit shutdown may cut that drain short.
    drop(frame_tx);
    tokio::select! {
        biased;
        _ = &mut *shutdown_rx => {
            let _ = timeout(SHUTDOWN_DISPATCH_GRACE, &mut dispatch).await;
            close_for_shutdown(socket).await;
            Ok(SessionEnd::Shutdown)
        }
        result = &mut dispatch => {
            result?;
            Ok(SessionEnd::Disconnected { clean })
        }
    }
}

async fn close_for_shutdown(socket: &mut WebSocketConnection) {
    let _ = timeout(SHUTDOWN_DISPATCH_GRACE, socket.close(None)).await;
}

fn is_fatal_session_error(error: &WebSocketError) -> bool {
    match error {
        WebSocketError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => false,
        WebSocketError::Io(error) if is_tls_io_error(error) => true,
        WebSocketError::Tls(_)
        | WebSocketError::Capacity(_)
        | WebSocketError::Protocol(_)
        | WebSocketError::Utf8
        | WebSocketError::AttackAttempt => true,
        _ => false,
    }
}

fn is_tls_io_error(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.downcast_ref::<rustls::Error>().is_some())
}

#[cfg(test)]
mod tests {
    use std::{io::ErrorKind, sync::Arc};

    use anyhow::Context as _;
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    use rustls::{
        pki_types::{CertificateDer, PrivatePkcs8KeyDer},
        CertificateError, ClientConfig, RootCertStore, ServerConfig,
    };
    use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
    use tokio_rustls::TlsAcceptor;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::error::{CapacityError, ProtocolError, TlsError};
    use tokio_tungstenite::tungstenite::http::{Response, StatusCode};

    use super::*;
    use crate::config::WebSocketSourceConfig;

    mod error_classification {
        use super::*;

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
        fn classifies_rustls_io_errors_during_a_session_as_fatal() {
            let tls_io = WebSocketError::Io(std::io::Error::new(
                ErrorKind::InvalidData,
                rustls::Error::General("invalid TLS record".to_string()),
            ));

            assert!(is_fatal_session_error(&tls_io));
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
                    connect_error_disposition(
                        &anyhow::Error::new(error).context("handshake failed")
                    ),
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
                    connect_error_disposition(
                        &anyhow::Error::new(error).context("handshake failed")
                    ),
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
    }

    mod shutdown {
        use super::*;

        #[tokio::test]
        async fn drain_includes_the_frame_waiting_for_capacity() {
            let (frame_tx, mut frame_rx) = mpsc::channel(1);
            frame_tx.try_send("queued".to_string()).unwrap();
            let dispatch = async move {
                assert_eq!(frame_rx.recv().await.as_deref(), Some("queued"));
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert_eq!(frame_rx.recv().await.as_deref(), Some("pending"));
                assert!(frame_rx.recv().await.is_none());
                Ok(())
            };
            tokio::pin!(dispatch);

            timeout(
                SHUTDOWN_DISPATCH_GRACE,
                drain_dispatch(frame_tx, Some("pending".to_string()), dispatch.as_mut()),
            )
            .await
            .expect("dispatch should drain within the shutdown grace period")
            .unwrap();
        }
    }

    mod tls {
        use super::*;

        #[tokio::test]
        async fn connects_with_a_trusted_certificate() {
            let (address, certificate, _accepted, server) = spawn_tls_server("127.0.0.1").await;
            let config = tls_config(format!("wss://{address}/events"));

            let socket = connect_inner_with_connector(
                &config,
                Some(tls_connector(std::slice::from_ref(&certificate))),
            )
            .await
            .unwrap();
            drop(socket);

            timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }

        #[tokio::test]
        async fn production_connector_rejects_self_signed_certificate_as_unknown_issuer() {
            let (address, _certificate, accepted, server) = spawn_tls_server("127.0.0.1").await;
            let config = tls_config(format!("wss://{address}/events"));

            let (client_result, accepted_result) = tokio::join!(
                timeout(Duration::from_secs(2), connect_inner(&config)),
                timeout(Duration::from_secs(2), accepted),
            );
            accepted_result
                .expect("server must accept TCP before the TLS handshake")
                .expect("server acceptance signal must remain connected");
            let error = client_result
                .expect("production TLS connection must finish")
                .unwrap_err();

            match rustls_error_in_chain(&error) {
                Some(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer)) => {}
                other => {
                    panic!("expected unknown-issuer certificate error, got {other:?}: {error:#?}")
                }
            }
            assert_eq!(
                safe_error_description(&error),
                "WebSocket TLS negotiation failed"
            );
            assert_eq!(
                connect_error_disposition(&error),
                ConnectErrorDisposition::Fatal
            );

            let server_error = timeout(Duration::from_secs(2), server)
                .await
                .expect("TLS server must finish")
                .expect("TLS server task must not panic")
                .expect_err("server must observe a failed TLS handshake");
            assert!(
                rustls_error_in_chain(&server_error).is_some(),
                "server failure must come from TLS negotiation: {server_error:#}"
            );
        }

        #[tokio::test]
        async fn rejects_an_untrusted_certificate_with_injected_roots() {
            let (address, _certificate, _accepted, server) = spawn_tls_server("127.0.0.1").await;
            let config = tls_config(format!("wss://{address}/events"));

            let error = connect_inner_with_connector(&config, Some(tls_connector(&[])))
                .await
                .unwrap_err();
            assert_eq!(
                safe_error_description(&error),
                "WebSocket TLS negotiation failed"
            );
            assert_eq!(
                connect_error_disposition(&error),
                ConnectErrorDisposition::Fatal
            );
            assert!(timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap()
                .is_err());
        }

        #[tokio::test]
        async fn rejects_a_hostname_mismatch() {
            let (address, certificate, _accepted, server) = spawn_tls_server("localhost").await;
            let config = tls_config(format!("wss://{address}/events"));

            let error = connect_inner_with_connector(
                &config,
                Some(tls_connector(std::slice::from_ref(&certificate))),
            )
            .await
            .unwrap_err();
            assert_eq!(
                safe_error_description(&error),
                "WebSocket TLS negotiation failed"
            );
            assert_eq!(
                connect_error_disposition(&error),
                ConnectErrorDisposition::Fatal
            );
            assert!(timeout(Duration::from_secs(2), server)
                .await
                .unwrap()
                .unwrap()
                .is_err());
        }
    }

    async fn spawn_tls_server(
        subject_alt_name: &str,
    ) -> (
        std::net::SocketAddr,
        CertificateDer<'static>,
        oneshot::Receiver<()>,
        JoinHandle<Result<()>>,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec![subject_alt_name.to_string()]).unwrap();
        let certificate = cert.der().clone();
        let private_key = PrivatePkcs8KeyDer::from(key_pair.serialize_der());
        let server_config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key.into())
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let _ = accepted_tx.send(());
            let tls = acceptor.accept(stream).await?;
            let mut socket = accept_async(tls).await?;
            socket.close(None).await?;
            Ok(())
        });

        (address, certificate, accepted_rx, server)
    }

    fn rustls_error_in_chain(error: &anyhow::Error) -> Option<&rustls::Error> {
        error.chain().find_map(|source| {
            if let Some(error) = source.downcast_ref::<rustls::Error>() {
                return Some(error);
            }
            if let Some(WebSocketError::Io(error)) = source.downcast_ref::<WebSocketError>() {
                return error
                    .get_ref()
                    .and_then(|source| source.downcast_ref::<rustls::Error>());
            }
            if let Some(WebSocketError::Tls(TlsError::Rustls(error))) =
                source.downcast_ref::<WebSocketError>()
            {
                return Some(error);
            }
            source
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .and_then(|source| source.downcast_ref::<rustls::Error>())
        })
    }

    fn tls_connector(certificates: &[CertificateDer<'static>]) -> Connector {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = RootCertStore::empty();
        for certificate in certificates {
            roots.add(certificate.clone()).unwrap();
        }
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Connector::Rustls(Arc::new(config))
    }

    fn tls_config(url: String) -> WebSocketSourceConfig {
        WebSocketSourceConfig {
            url,
            ..Default::default()
        }
    }
}
