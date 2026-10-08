// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::{MemoryStateStoreProvider, Reaction};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Semaphore};

use crate::{AdaptiveBatchConfig, HttpReaction};

#[path = "../../tests/lifecycle_support.rs"]
mod support;

struct Receiver {
    url: String,
    requests: mpsc::Receiver<serde_json::Value>,
    release: Arc<Semaphore>,
    worker: tokio::task::JoinHandle<()>,
}

async fn receiver(request_count: usize) -> Receiver {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (requests, rx) = mpsc::channel(request_count);
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let worker = tokio::spawn(async move {
        for _ in 0..request_count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (_, body) = support::read_http_request(&mut socket).await;
            requests.send(body).await.unwrap();
            gate.acquire().await.unwrap().forget();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        }
    });
    Receiver {
        url,
        requests: rx,
        release,
        worker,
    }
}

fn reaction(url: &str, adaptive: bool) -> HttpReaction {
    let mut builder = HttpReaction::builder("http-lifecycle")
        .with_query("q1")
        .with_base_url(url)
        .with_timeout_ms(30_000);
    if adaptive {
        builder = builder
            .with_adaptive(AdaptiveBatchConfig {
                adaptive_min_batch_size: 1,
                adaptive_max_batch_size: 1,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 10,
            })
            .with_batch_endpoint("/batch");
    }
    builder.build().unwrap()
}

async fn arrived(server: &mut Receiver, adaptive: bool, sequence: u64) {
    let payload = tokio::time::timeout(Duration::from_secs(3), server.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let notification = if adaptive {
        assert_eq!(payload["batch"].as_array().unwrap().len(), 1);
        &payload["batch"][0]
    } else {
        &payload
    };
    assert_eq!(notification["queryId"], "q1");
    assert_eq!(notification["sequenceId"], sequence);
    assert_eq!(notification["after"]["generation"], sequence);
}

#[tokio::test(flavor = "current_thread")]
async fn requests_survive_stop_and_three_restarts_in_both_modes() {
    for adaptive in [false, true] {
        let mut server = receiver(3).await;
        let reaction = reaction(&server.url, adaptive);
        support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
        for sequence in 1..=3 {
            reaction.start().await.unwrap();
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .is::<drasi_lib::context::workers::WorkerAlreadyOwned>());
            reaction
                .enqueue_query_result(support::result(sequence))
                .await
                .unwrap();
            arrived(&mut server, adaptive, sequence).await;
            if sequence == 1 {
                support::assert_incomplete_stop(&reaction, &reaction.base).await;
                assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
            }
            server.release.add_permits(1);
            reaction.stop().await.unwrap();
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.base.shutdown_tx.read().await.is_none());
            assert_eq!(
                reaction
                    .base
                    .read_checkpoint("q1")
                    .await
                    .unwrap()
                    .unwrap()
                    .sequence,
                sequence
            );
        }
        server.worker.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn checkpoint_writes_survive_stop_in_both_modes() {
    for adaptive in [false, true] {
        let mut server = receiver(1).await;
        let reaction = reaction(&server.url, adaptive);
        let store = Arc::new(support::ControlledStore::new(false));
        support::initialize(&reaction, store.clone()).await;
        reaction.start().await.unwrap();
        reaction
            .enqueue_query_result(support::result(7))
            .await
            .unwrap();
        arrived(&mut server, adaptive, 7).await;
        server.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), store.entered.notified())
            .await
            .unwrap();
        support::assert_incomplete_stop(&reaction, &reaction.base).await;
        assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
        store.release.add_permits(1);
        reaction.stop().await.unwrap();
        assert_eq!(
            reaction
                .base
                .read_checkpoint("q1")
                .await
                .unwrap()
                .unwrap()
                .sequence,
            7
        );
        server.worker.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_start_and_checkpoint_panic_require_cleanup_in_both_modes() {
    for adaptive in [false, true] {
        let mut server = receiver(1).await;
        let reaction = reaction(&server.url, adaptive);
        let store = Arc::new(support::ControlledStore::new(true));
        support::initialize(&reaction, store.clone()).await;
        support::assert_cancelled_start(&reaction, &reaction.base).await;
        reaction.start().await.unwrap();
        reaction
            .enqueue_query_result(support::result(1))
            .await
            .unwrap();
        arrived(&mut server, adaptive, 1).await;
        server.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), store.entered.notified())
            .await
            .unwrap();
        store.release.add_permits(1);
        support::assert_panic_cleanup(&reaction, &reaction.base).await;
        assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
        server.worker.await.unwrap();
    }
}
