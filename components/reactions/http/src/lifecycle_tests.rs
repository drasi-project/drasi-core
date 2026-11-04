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

#[path = "../../tests/worker_telemetry.rs"]
mod worker_telemetry;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_tracing_preserves_origins_until_actual_dispatch_in_both_modes() {
    use drasi_lib::observability::ExecutionContext;
    for adaptive in [false, true] {
        let request_count = if adaptive { 2 } else { 4 };
        let mut server = receiver(request_count).await;
        let mut builder = HttpReaction::builder("http-trace")
            .with_query("q1")
            .with_base_url(&server.url);
        if adaptive {
            builder = builder
                .with_adaptive(AdaptiveBatchConfig {
                    adaptive_min_batch_size: 2,
                    adaptive_max_batch_size: 2,
                    adaptive_window_size: 10,
                    adaptive_batch_timeout_ms: 10_000,
                })
                .with_batch_endpoint("/batch");
        }
        let reaction = Arc::new(builder.build().unwrap());
        let recording = worker_telemetry::Recording::default();
        let origins = [
            worker_telemetry::context(1),
            worker_telemetry::context(2),
            ExecutionContext::default(),
            ExecutionContext::default(),
        ];
        recording
            .run(reaction.clone(), async {
                for (index, origin) in origins.iter().enumerate() {
                    let mut result = support::result(index as u64 + 1);
                    if index == 1 {
                        result.sequence = 1;
                    }
                    worker_telemetry::enqueue(reaction.as_ref(), result, origin.clone()).await;
                }
                let first = tokio::time::timeout(Duration::from_secs(3), server.requests.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    recording.finished().is_empty(),
                    "dispatch has not completed"
                );
                let first = if adaptive { &first["batch"][0] } else { &first };
                assert!(first.get("executionContext").is_none());
                assert_eq!(first["queryId"], "q1");
                server.release.add_permits(request_count);
                for _ in 1..request_count {
                    tokio::time::timeout(Duration::from_secs(3), server.requests.recv())
                        .await
                        .unwrap()
                        .unwrap();
                }
                recording.wait_for_finished(request_count).await;

                let mut empty = support::result(100);
                empty.results.clear();
                worker_telemetry::enqueue(reaction.as_ref(), empty, worker_telemetry::context(3))
                    .await;
                let mut noop = support::result(101);
                noop.results = vec![drasi_lib::channels::ResultDiff::Noop];
                worker_telemetry::enqueue(reaction.as_ref(), noop, worker_telemetry::context(4))
                    .await;
                tokio::time::timeout(Duration::from_secs(3), async {
                    while !reaction.base.priority_queue.is_empty().await {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
            })
            .await;
        server.worker.await.unwrap();

        let spans = recording.finished();
        assert_eq!(spans.len(), request_count);
        for span in &spans {
            assert_eq!(
                span.name,
                if adaptive {
                    "reaction.process_batch"
                } else {
                    "reaction.process"
                }
            );
            assert_eq!(span.fields["component_id"], "http-trace");
            assert_eq!(span.fields["outcome"], "success");
            assert_eq!(span.fields["completion"], "handled");
            assert_eq!(span.fields["input_count"], if adaptive { "2" } else { "1" });
            assert_eq!(
                span.fields["change_count"],
                if adaptive { "2" } else { "1" }
            );
        }
        let parents = recording.parents();
        if adaptive {
            assert!(parents[0].context.is_empty());
            assert_eq!(parents[0].links.len(), 2);
            assert!(parents[0].links.contains(origins[0].trace().unwrap()));
            assert!(parents[0].links.contains(origins[1].trace().unwrap()));
            assert!(parents[1].context.is_empty());
            assert!(parents[1].links.is_empty());
        } else {
            assert_eq!(
                parents
                    .iter()
                    .map(|parent| parent.context.clone())
                    .collect::<Vec<_>>(),
                origins
            );
            assert!(parents.iter().all(|parent| parent.links.is_empty()));
        }
    }
}

#[tokio::test]
async fn worker_tracing_records_http_drops_and_failures_without_changing_policy() {
    use drasi_lib::{channels::ComponentStatus, ReactionRecoveryPolicy};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    for adaptive in [false, true] {
        for (status, policy) in [
            (400, ReactionRecoveryPolicy::Strict),
            (503, ReactionRecoveryPolicy::AutoSkipGap),
            (503, ReactionRecoveryPolicy::Strict),
        ] {
            let server = MockServer::start().await;
            Mock::given(wiremock::matchers::method("POST"))
                .respond_with(ResponseTemplate::new(status))
                .mount(&server)
                .await;
            let mut builder = HttpReaction::builder("http-trace-error")
                .with_query("q1")
                .with_base_url(server.uri())
                .with_recovery_policy(policy);
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
            let reaction = Arc::new(builder.build().unwrap());
            let recording = worker_telemetry::Recording::default();
            recording
                .run(reaction.clone(), async {
                    worker_telemetry::enqueue(
                        reaction.as_ref(),
                        support::result(7),
                        worker_telemetry::context(1),
                    )
                    .await;
                    recording.wait_for_finished(1).await;
                    assert_eq!(recording.finished()[0].fields["outcome"], "error");
                    let checkpoint = reaction.base.read_checkpoint("q1").await.unwrap();
                    if status == 503 && policy == ReactionRecoveryPolicy::Strict {
                        assert!(checkpoint.is_none());
                        assert!(matches!(reaction.status().await, ComponentStatus::Error));
                    } else {
                        assert_eq!(checkpoint.unwrap().sequence, 7);
                        assert!(matches!(reaction.status().await, ComponentStatus::Running));
                    }
                })
                .await;
        }
    }
}

#[tokio::test]
async fn worker_tracing_records_cancellation_not_success() {
    let mut server = receiver(1).await;
    let reaction = Arc::new(reaction(&server.url, false));
    let recording = worker_telemetry::Recording::default();
    recording
        .run(reaction.clone(), async {
            worker_telemetry::enqueue(
                reaction.as_ref(),
                support::result(1),
                worker_telemetry::context(1),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(3), server.requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(recording.finished().is_empty());
            reaction
                .base
                .processing_task
                .read()
                .await
                .as_ref()
                .unwrap()
                .abort();
            recording.wait_for_finished(1).await;
            assert_eq!(recording.finished()[0].fields["outcome"], "cancelled");
            assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
            reaction.stop().await.unwrap();
            server.release.add_permits(1);
        })
        .await;
    let _ = server.worker.await;
}
