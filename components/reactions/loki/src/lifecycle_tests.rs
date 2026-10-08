// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{sync::Arc, time::Duration};

use drasi_lib::{channels::ComponentStatus, MemoryStateStoreProvider, Reaction};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, Semaphore},
};

use super::LokiReaction;
use crate::{QueryConfig, TemplateSpec};

#[path = "../../tests/lifecycle_support.rs"]
#[allow(dead_code)]
mod support;

struct Receiver {
    url: String,
    requests: mpsc::Receiver<serde_json::Value>,
    release: Arc<Semaphore>,
    worker: tokio::task::JoinHandle<()>,
}

async fn receiver(count: usize, error_body: bool) -> Receiver {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (requests, rx) = mpsc::channel(count);
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let worker = tokio::spawn(async move {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (header, body) = support::read_http_request(&mut socket).await;
            assert!(header.starts_with("POST /loki/api/v1/push HTTP/1.1\r\n"));
            assert!(header
                .lines()
                .any(|line| line.eq_ignore_ascii_case("authorization: Bearer test-token")));
            assert!(header
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-scope-orgid: test-tenant")));
            if error_body {
                socket.write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 12\r\nConnection: close\r\n\r\nheld "
                ).await.unwrap();
            }
            requests.send(body).await.unwrap();
            gate.acquire().await.unwrap().forget();
            if error_body {
                socket.write_all(b"failure").await.unwrap();
            } else {
                socket.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ).await.unwrap();
            }
        }
    });
    Receiver {
        url,
        requests: rx,
        release,
        worker,
    }
}

fn reaction(endpoint: &str) -> LokiReaction {
    LokiReaction::builder("loki-lifecycle")
        .with_query("q1")
        .with_endpoint(endpoint)
        .with_timeout_ms(30_000)
        .with_token("test-token")
        .with_tenant_id("test-tenant")
        .with_label("job", "lifecycle")
        .with_label("generation", "{{after.generation}}")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("{{json after}}")),
            ..Default::default()
        })
        .build()
        .unwrap()
}

async fn arrived(server: &mut Receiver, sequence: u64) {
    let payload = tokio::time::timeout(Duration::from_secs(5), server.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let streams = payload["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 1);
    let stream = &streams[0];
    assert_eq!(stream["stream"]["query_id"], "q1");
    assert_eq!(stream["stream"]["reaction_id"], "loki-lifecycle");
    assert_eq!(stream["stream"]["operation"], "ADD");
    assert_eq!(stream["stream"]["job"], "lifecycle");
    assert_eq!(stream["stream"]["generation"], sequence.to_string());
    let values = stream["values"].as_array().unwrap();
    assert_eq!(values.len(), 1);
    assert!(values[0][0].as_str().unwrap().parse::<i64>().unwrap() > 0);
    let row: serde_json::Value = serde_json::from_str(values[0][1].as_str().unwrap()).unwrap();
    assert_eq!(row["generation"], sequence);
}

#[tokio::test(flavor = "current_thread")]
async fn requests_and_error_bodies_remain_owned_through_stop_and_three_restarts() {
    for error_body in [false, true] {
        let mut server = receiver(3, error_body).await;
        let reaction = reaction(&server.url);
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
            arrived(&mut server, sequence).await;
            if sequence == 1 {
                support::assert_incomplete_stop(&reaction, &reaction.base).await;
            }
            server.release.add_permits(1);
            reaction.stop().await.unwrap();
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.base.shutdown_tx.read().await.is_none());
            assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        }
        server.worker.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn stopping_status_does_not_discard_admitted_input_before_shutdown_signal() {
    let mut server = receiver(1, false).await;
    let reaction = reaction(&server.url);
    support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
    reaction.start().await.unwrap();
    let signalling = reaction.base.shutdown_tx.write().await;
    let stop = reaction.stop();
    tokio::pin!(stop);
    tokio::select! {
        result = &mut stop => panic!("cleanup bypassed held shutdown signal: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }
    assert_eq!(reaction.status().await, ComponentStatus::Stopping);
    reaction
        .enqueue_query_result(support::result(1))
        .await
        .unwrap();
    arrived(&mut server, 1).await;
    drop(signalling);
    server.release.add_permits(1);
    stop.await.unwrap();
    server.worker.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn startup_registration_cancellation_and_worker_panic_preserve_cleanup_fence() {
    let reaction = reaction("http://127.0.0.1:1");
    support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
    support::assert_cancelled_start(&reaction, &reaction.base).await;
    let registration = reaction.base.processing_task.read().await;
    {
        let start = reaction.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => panic!("start bypassed owned registration: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert!(registration.is_none());
        assert_eq!(reaction.status().await, ComponentStatus::Starting);
        drop(registration);
        start.await.unwrap();
    }
    reaction.stop().await.unwrap();
    drasi_lib::context::workers::spawn_owned_worker(&reaction.base.processing_task, async {
        panic!("injected Loki processor panic")
    })
    .await
    .unwrap();
    support::assert_panic_cleanup(&reaction, &reaction.base).await;
}
