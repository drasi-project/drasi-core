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

use super::*;
use crate::evaluation::context::QueryPartEvaluationContext;
use crate::evaluation::variable_value::VariableValue;
use crate::interface::FutureQueueConsumer;
use crate::query::{AutoFutureQueueConsumer, ContinuousQuery};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

async fn query(function: &str) -> Arc<ContinuousQuery> {
    let registry = Arc::new(FunctionRegistry::new());
    Arc::new(
        QueryBuilder::new(
            format!("MATCH (r:Request) WHERE drasi.{function}(r.pending, r.due) RETURN r.id AS id"),
            Arc::new(CypherParser::new(registry)),
        )
        .build()
        .await,
    )
}

fn request(time: u64, due: u64, pending: serde_json::Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", "r1"),
            labels: Arc::new([Arc::from("Request")]),
            effective_from: time,
        },
        properties: json!({"id": "r1", "pending": pending, "due": due}).into(),
    }
}

#[tokio::test]
async fn reschedule_holds_change_lock_before_due_selection() {
    let cq = query("trueLater").await;
    cq.process_source_change(SourceChange::Insert {
        element: request(0, 100, json!(true)),
    })
    .await
    .unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(100));

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let updating = cq.clone();
    let update = tokio::spawn(async move {
        updating
            .process_source_change_with_hook(
                SourceChange::Update {
                    element: request(50, 200, json!(true)),
                },
                || async {
                    entered_tx.send(()).unwrap();
                    release_rx.await.unwrap();
                    Ok(())
                },
            )
            .await
            .unwrap()
    });
    entered_rx.await.unwrap();
    let stale_wakeup = cq.process_due_futures_at(100);
    tokio::pin!(stale_wakeup);
    assert!(
        futures::poll!(&mut stale_wakeup).is_pending(),
        "selection must wait for the source-change lock"
    );
    release_tx.send(()).unwrap();
    assert!(update.await.unwrap().is_empty());
    assert!(stale_wakeup.await.unwrap().is_none());
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(200));
    let result = cq.process_due_futures_at(200).await.unwrap().unwrap();
    assert_eq!(result.source_id.as_ref(), "source");
    assert!(
        matches!(&result.results[..], [QueryPartEvaluationContext::Adding { after, .. }]
        if after["id"] == VariableValue::String("r1".into()))
    );
    assert!(cq.process_due_futures_at(200).await.unwrap().is_none());
}

#[tokio::test]
async fn true_later_and_true_until_past_equal_future_boundaries() {
    for function in ["trueLater", "trueUntil"] {
        for due in [99, 100, 101] {
            let cq = query(function).await;
            let result = cq
                .process_source_change(SourceChange::Insert {
                    element: request(100, due, json!(true)),
                })
                .await
                .unwrap();
            if due <= 100 {
                assert_eq!(result.len(), 1, "{function} at {due}");
                assert!(cq.process_due_futures_at(100).await.unwrap().is_none());
            } else {
                assert!(result.is_empty());
                assert!(cq.process_due_futures_at(100).await.unwrap().is_none());
                assert_eq!(
                    cq.process_due_futures_at(101)
                        .await
                        .unwrap()
                        .unwrap()
                        .results
                        .len(),
                    1
                );
                assert!(cq.process_due_futures_at(102).await.unwrap().is_none());
            }
        }
    }
}

#[tokio::test]
async fn true_until_false_cancels_but_null_is_not_cancellation() {
    let cq = query("trueUntil").await;
    cq.process_source_change(SourceChange::Insert {
        element: request(0, 100, json!(true)),
    })
    .await
    .unwrap();
    cq.process_source_change(SourceChange::Update {
        element: request(10, 100, json!(null)),
    })
    .await
    .unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(100));
    cq.process_source_change(SourceChange::Update {
        element: request(20, 100, json!(false)),
    })
    .await
    .unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), None);
    assert!(cq.process_due_futures_at(100).await.unwrap().is_none());
}

#[tokio::test]
async fn auto_consumer_uses_its_clock_for_stale_signals() {
    let cq = query("trueLater").await;
    let now = Arc::new(AtomicU64::new(99));
    let consumer = AutoFutureQueueConsumer::new(cq.clone()).with_now_override(now.clone());
    cq.process_source_change(SourceChange::Insert {
        element: request(0, 100, json!(true)),
    })
    .await
    .unwrap();
    consumer.on_items_due().await.unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(100));
    now.store(100, Ordering::Relaxed);
    consumer.on_items_due().await.unwrap();
    assert_eq!(
        consumer.recv(Duration::from_secs(1)).await.unwrap().len(),
        1
    );
    consumer.on_items_due().await.unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), None);
}

#[tokio::test]
async fn true_for_keeps_first_true_time_and_false_restarts_it() {
    let cq = query("trueFor").await;
    for change in [
        SourceChange::Insert {
            element: request(0, 100, json!(true)),
        },
        SourceChange::Update {
            element: request(20, 100, json!(true)),
        },
    ] {
        assert!(cq.process_source_change(change).await.unwrap().is_empty());
        assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(100));
    }
    cq.process_source_change(SourceChange::Update {
        element: request(50, 100, json!(false)),
    })
    .await
    .unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), None);
    cq.process_source_change(SourceChange::Update {
        element: request(60, 100, json!(true)),
    })
    .await
    .unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(160));
    assert!(cq.process_due_futures_at(159).await.unwrap().is_none());
    assert_eq!(
        cq.process_due_futures_at(160)
            .await
            .unwrap()
            .unwrap()
            .results
            .len(),
        1
    );
    assert!(cq.process_due_futures_at(260).await.unwrap().is_none());
}

#[tokio::test]
async fn true_now_or_later_true_is_immediate_false_waits() {
    let cq = query("trueNowOrLater").await;
    assert_eq!(
        cq.process_source_change(SourceChange::Insert {
            element: request(0, 100, json!(true))
        })
        .await
        .unwrap()
        .len(),
        1
    );
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), None);
    assert_eq!(
        cq.process_source_change(SourceChange::Update {
            element: request(10, 100, json!(false))
        })
        .await
        .unwrap()
        .len(),
        1
    );
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(100));
    assert!(cq.process_due_futures_at(99).await.unwrap().is_none());
    assert!(cq
        .process_due_futures_at(100)
        .await
        .unwrap()
        .unwrap()
        .results
        .is_empty());
    assert!(cq.process_due_futures_at(101).await.unwrap().is_none());
}

#[tokio::test]
async fn true_later_false_does_not_cancel_but_never_activates() {
    let cq = query("trueLater").await;
    cq.process_source_change(SourceChange::Insert {
        element: request(0, 100, json!(true)),
    })
    .await
    .unwrap();
    cq.process_source_change(SourceChange::Update {
        element: request(10, 200, json!(false)),
    })
    .await
    .unwrap();
    assert_eq!(cq.future_queue().peek_due_time().await.unwrap(), Some(200));
    assert!(cq.process_due_futures_at(100).await.unwrap().is_none());
    assert!(cq
        .process_due_futures_at(200)
        .await
        .unwrap()
        .unwrap()
        .results
        .is_empty());
    assert!(cq.process_due_futures_at(200).await.unwrap().is_none());
}
