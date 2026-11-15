// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A connection-limited TCP incoming stream for the tonic server.
//!
//! Each accepted connection holds a permit until it closes. Connections beyond
//! `maxConnections` are closed immediately. Accept errors (for example running
//! out of file descriptors) are logged and retried after a short pause.

use drasi_computation_network_common::diag;
use futures_util::Stream;
use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use tonic::transport::server::{Connected, TcpConnectInfo};

const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

/// An accepted connection that releases its slot when dropped.
pub(crate) struct LimitedStream {
    inner: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl Connected for LimitedStream {
    type ConnectInfo = TcpConnectInfo;
    fn connect_info(&self) -> Self::ConnectInfo {
        self.inner.connect_info()
    }
}

impl AsyncRead for LimitedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for LimitedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Accepts connections up to `max_connections` at a time.
pub(crate) fn limited(
    listener: TcpListener,
    max_connections: usize,
) -> impl Stream<Item = io::Result<LimitedStream>> {
    let slots = Arc::new(Semaphore::new(max_connections));
    async_stream::stream! {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let Ok(permit) = slots.clone().try_acquire_owned() else {
                        diag!(Debug, "gRPC connection limit reached; closing connection from {peer}");
                        drop(stream);
                        continue;
                    };
                    let _ = stream.set_nodelay(true);
                    yield Ok(LimitedStream { inner: stream, _permit: permit });
                }
                Err(error) => {
                    diag!(Warn, "gRPC accept failed: {error}");
                    tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                }
            }
        }
    }
}
