// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The connection-limited incoming stream for the tonic server.
//!
//! Each accepted connection holds a permit until it closes; connections beyond
//! `maxConnections` are closed immediately. A connection must finish its TLS
//! handshake (when TLS is configured) and send the HTTP/2 connection preface
//! within `headerReadTimeoutMs`, so idle or stalled sockets cannot occupy
//! every slot. This matches the HTTP source's header and handshake deadline.
//! Handshakes run concurrently, so one slow client never delays accepting
//! another. Accept errors (for example running out of file descriptors) are
//! logged and retried after a short pause.

use drasi_computation_network_common::{diag, tokio_rustls::TlsAcceptor};
use futures_util::Stream;
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::{Instant, Sleep},
};
use tonic::transport::server::{Connected, TcpConnectInfo};

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// The HTTP/2 client connection preface (RFC 9113 section 3.4).
const PREFACE_BYTES: usize = 24;

type TlsStream = drasi_computation_network_common::tokio_rustls::server::TlsStream<TcpStream>;

enum Transport {
    Plain(TcpStream),
    Tls(Box<TlsStream>),
}

/// An accepted connection that releases its slot when dropped and fails its
/// reads if the HTTP/2 preface does not arrive before the deadline.
pub(crate) struct LimitedStream {
    inner: Transport,
    info: TcpConnectInfo,
    preface: Option<Pin<Box<Sleep>>>,
    received: usize,
    _permit: OwnedSemaphorePermit,
}

impl Connected for LimitedStream {
    type ConnectInfo = TcpConnectInfo;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.info.clone()
    }
}

macro_rules! transport {
    ($self:ident, $stream:ident => $body:expr) => {
        match &mut $self.inner {
            Transport::Plain($stream) => {
                let $stream = Pin::new($stream);
                $body
            }
            Transport::Tls($stream) => {
                let $stream = Pin::new($stream.as_mut());
                $body
            }
        }
    };
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        let before = buf.filled().len();
        let polled = transport!(this, stream => stream.poll_read(cx, buf));
        if let Some(deadline) = this.preface.as_mut() {
            match polled {
                Poll::Ready(Ok(())) => {
                    this.received += buf.filled().len() - before;
                    if this.received >= PREFACE_BYTES {
                        this.preface = None;
                    }
                }
                Poll::Pending => {
                    ready!(deadline.as_mut().poll(cx));
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "HTTP/2 connection preface not received before headerReadTimeoutMs",
                    )));
                }
                Poll::Ready(Err(_)) => {}
            }
        }
        polled
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        transport!(self, stream => stream.poll_write(cx, buf))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        transport!(self, stream => stream.poll_flush(cx))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        transport!(self, stream => stream.poll_shutdown(cx))
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        transport!(self, stream => stream.poll_write_vectored(cx, bufs))
    }
    fn is_write_vectored(&self) -> bool {
        match &self.inner {
            Transport::Plain(stream) => stream.is_write_vectored(),
            Transport::Tls(stream) => stream.is_write_vectored(),
        }
    }
}

/// Listener settings for [`limited`].
pub(crate) struct IncomingOptions {
    pub tls: Option<Arc<drasi_computation_network_common::tokio_rustls::rustls::ServerConfig>>,
    pub max_connections: usize,
    /// Deadline for the TLS handshake and the HTTP/2 connection preface.
    pub header_read_timeout: Duration,
}

enum Event {
    Accepted(io::Result<(TcpStream, std::net::SocketAddr)>),
    Ready(Option<LimitedStream>),
}

/// Accepts connections up to `max_connections` at a time.
pub(crate) fn limited(
    listener: TcpListener,
    options: IncomingOptions,
) -> impl Stream<Item = io::Result<LimitedStream>> {
    let slots = Arc::new(Semaphore::new(options.max_connections));
    let acceptor = options.tls.map(TlsAcceptor::from);
    let timeout = options.header_read_timeout;
    async_stream::stream! {
        // Dropped with the stream, which aborts handshakes still in progress.
        let mut handshakes = JoinSet::new();
        loop {
            let event = tokio::select! {
                accepted = listener.accept() => Event::Accepted(accepted),
                Some(done) = handshakes.join_next(), if !handshakes.is_empty() => {
                    Event::Ready(done.ok().flatten())
                }
            };
            match event {
                Event::Accepted(Ok((stream, peer))) => {
                    let Ok(permit) = slots.clone().try_acquire_owned() else {
                        diag!(Debug, "gRPC connection limit reached; closing connection from {peer}");
                        drop(stream);
                        continue;
                    };
                    let _ = stream.set_nodelay(true);
                    let info = stream.connect_info();
                    let deadline = Instant::now() + timeout;
                    let connection = move |inner| LimitedStream {
                        inner,
                        info,
                        preface: Some(Box::pin(tokio::time::sleep_until(deadline))),
                        received: 0,
                        _permit: permit,
                    };
                    match &acceptor {
                        None => yield Ok(connection(Transport::Plain(stream))),
                        Some(acceptor) => {
                            let accept = acceptor.accept(stream);
                            handshakes.spawn(async move {
                                match tokio::time::timeout_at(deadline, accept).await {
                                    Ok(Ok(stream)) => Some(connection(Transport::Tls(Box::new(stream)))),
                                    Ok(Err(error)) => {
                                        diag!(Debug, "gRPC TLS handshake with {peer} failed: {error}");
                                        None
                                    }
                                    Err(_) => {
                                        diag!(Debug, "gRPC TLS handshake with {peer} timed out");
                                        None
                                    }
                                }
                            });
                        }
                    }
                }
                Event::Accepted(Err(error)) => {
                    diag!(Warn, "gRPC accept failed: {error}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
                Event::Ready(Some(connection)) => yield Ok(connection),
                Event::Ready(None) => {}
            }
        }
    }
}
