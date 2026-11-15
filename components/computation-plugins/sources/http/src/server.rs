// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The HTTP listener: accepts connections (optionally over TLS), bounds them,
//! serves the router with HTTP/1.1 and HTTP/2, and drains gracefully.
//!
//! Every connection task is owned by the serving future. Cancelling the
//! activation stops accepting and lets in-flight requests finish (Drain);
//! aborting the serving task drops all connection tasks (Abort).

use axum::Router;
use drasi_computation_network_common::diag;
use hyper::{body::Incoming, Request};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::{conn::auto, graceful::GracefulShutdown},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

/// The client address of a request, inserted by the listener.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PeerAddress(pub SocketAddr);

pub(crate) struct ServerOptions {
    pub tls: Option<Arc<rustls_config::ServerConfig>>,
    pub max_connections: usize,
    pub header_read_timeout: Duration,
}

/// Re-exported so the component can name the TLS configuration type without
/// a direct rustls dependency.
pub(crate) mod rustls_config {
    pub use tokio_rustls::rustls::ServerConfig;
}

/// Pause between accept attempts after an accept error, such as running out
/// of file descriptors, so a persistent error cannot spin the loop.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(50);

pub(crate) async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: impl std::future::Future<Output = ()> + Send,
    options: ServerOptions,
) -> anyhow::Result<()> {
    tokio::pin!(shutdown);
    let graceful = GracefulShutdown::new();
    let connections = Arc::new(Semaphore::new(options.max_connections));
    let acceptor = options.tls.map(TlsAcceptor::from);
    let mut builder = auto::Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(options.header_read_timeout)
        .keep_alive(true);
    builder
        .http2()
        .timer(TokioTimer::new())
        .keep_alive_interval(Some(Duration::from_secs(30)))
        .keep_alive_timeout(Duration::from_secs(20));
    let builder = Arc::new(builder);
    let mut tasks = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };
        // Reap finished connection tasks so the set stays bounded.
        while tasks.try_join_next().is_some() {}
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(error) => {
                diag!(Warn, "HTTP accept failed: {error}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            diag!(
                Debug,
                "HTTP connection limit reached; closing connection from {peer}"
            );
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let router = router.clone();
        let builder = builder.clone();
        let watcher = graceful.watcher();
        let acceptor = acceptor.clone();
        let handshake_timeout = options.header_read_timeout;
        tasks.spawn(async move {
            let _permit = permit;
            let service = hyper::service::service_fn(move |mut request: Request<Incoming>| {
                request.extensions_mut().insert(PeerAddress(peer));
                router.clone().oneshot(request)
            });
            let result = match acceptor {
                Some(acceptor) => {
                    match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
                        Ok(Ok(stream)) => {
                            let connection = builder
                                .serve_connection_with_upgrades(TokioIo::new(stream), service);
                            watcher.watch(connection).await
                        }
                        Ok(Err(error)) => {
                            diag!(Debug, "TLS handshake with {peer} failed: {error}");
                            return;
                        }
                        Err(_) => {
                            diag!(Debug, "TLS handshake with {peer} timed out");
                            return;
                        }
                    }
                }
                None => {
                    let connection =
                        builder.serve_connection_with_upgrades(TokioIo::new(stream), service);
                    watcher.watch(connection).await
                }
            };
            if let Err(error) = result {
                diag!(
                    Debug,
                    "HTTP connection from {peer} ended with an error: {error}"
                );
            }
        });
    }
    drop(listener);
    // Drain: in-flight requests finish, idle connections close.
    graceful.shutdown().await;
    while tasks.join_next().await.is_some() {}
    Ok(())
}
