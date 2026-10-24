// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashSet, sync::Arc, time::Duration};

use drasi_lib::{channels::ComponentStatus, MemoryStateStoreProvider, Reaction};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, Semaphore},
};

use super::SqsReaction;
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

async fn receiver(count: usize) -> Receiver {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, requests) = mpsc::channel(count);
    let release = Arc::new(Semaphore::new(0));
    let gate = release.clone();
    let worker = tokio::spawn(async move {
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (header, body) = support::read_http_request(&mut socket).await;
            assert!(header
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-amz-target: AmazonSQS.SendMessage")));
            sender.send(body).await.unwrap();
            gate.acquire().await.unwrap().forget();
            let body = br#"{"MessageId":"00000000-0000-0000-0000-000000000001"}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/x-amz-json-1.0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(body).await.unwrap();
        }
    });
    Receiver {
        url,
        requests,
        release,
        worker,
    }
}

fn reaction(endpoint: &str, fifo: bool) -> SqsReaction {
    SqsReaction::builder("sqs-lifecycle")
        .with_query("q1")
        .with_queue_url(format!(
            "{endpoint}/queue{}",
            if fifo { ".fifo" } else { "" }
        ))
        .with_endpoint_url(endpoint)
        .with_region("us-east-1")
        .with_credentials("test", "test")
        .with_fifo_queue(fifo)
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("{{json after}}")),
            ..Default::default()
        })
        .build()
        .unwrap()
}

async fn arrived(server: &mut Receiver, fifo: bool, sequence: u64) -> Option<String> {
    let payload = tokio::time::timeout(Duration::from_secs(5), server.requests.recv())
        .await
        .unwrap()
        .unwrap();
    let body: serde_json::Value =
        serde_json::from_str(payload["MessageBody"].as_str().unwrap()).unwrap();
    assert_eq!(body["generation"], sequence);
    assert_eq!(
        payload["MessageAttributes"]["drasi-query-id"]["StringValue"],
        "q1"
    );
    assert_eq!(
        payload["MessageAttributes"]["drasi-operation"]["StringValue"],
        "ADD"
    );
    assert_eq!(
        payload["QueueUrl"],
        format!("{}/queue{}", server.url, if fifo { ".fifo" } else { "" })
    );
    if fifo {
        assert_eq!(payload["MessageGroupId"], "q1");
        let id = payload["MessageDeduplicationId"].as_str().unwrap();
        uuid::Uuid::parse_str(id).unwrap();
        Some(id.to_owned())
    } else {
        assert!(payload.get("MessageGroupId").is_none());
        assert!(payload.get("MessageDeduplicationId").is_none());
        None
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_requests_survive_stop_and_three_restarts_for_standard_and_fifo_queues() {
    for fifo in [false, true] {
        let mut server = receiver(3).await;
        let reaction = reaction(&server.url, fifo);
        support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
        let mut ids = HashSet::new();
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
            if let Some(id) = arrived(&mut server, fifo, sequence).await {
                assert!(ids.insert(id));
            }
            if sequence == 1 {
                support::assert_incomplete_stop(&reaction, &reaction.base).await;
            }
            server.release.add_permits(1);
            tokio::time::timeout(Duration::from_secs(5), reaction.stop())
                .await
                .unwrap()
                .unwrap();
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.base.shutdown_tx.read().await.is_none());
            assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        }
        server.worker.await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn running_waits_for_owned_registration_and_cancelled_start_requires_cleanup() {
    let mut server = receiver(1).await;
    let reaction = reaction(&server.url, false);
    support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
    support::assert_cancelled_start(&reaction, &reaction.base).await;
    let registration = reaction.base.processing_task.read().await;
    reaction
        .enqueue_query_result(support::result(1))
        .await
        .unwrap();
    {
        let start = reaction.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => panic!("start bypassed registration lock: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        assert_eq!(reaction.status().await, ComponentStatus::Starting);
        assert!(registration.is_none());
        assert!(server.requests.try_recv().is_err());
        drop(registration);
        tokio::time::timeout(Duration::from_secs(5), start)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(reaction.status().await, ComponentStatus::Running);
    arrived(&mut server, false, 1).await;
    server.release.add_permits(1);
    reaction.stop().await.unwrap();
    server.worker.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn worker_panic_and_remaining_base_cleanup_block_restart() {
    let reaction = reaction("http://127.0.0.1:1", false);
    support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
    drasi_lib::context::workers::spawn_owned_worker(&reaction.base.processing_task, async {
        panic!("injected SQS processor panic")
    })
    .await
    .unwrap();
    support::assert_panic_cleanup(&reaction, &reaction.base).await;
}
