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

use crate::evaluation::QueryExecutionError;

use crate::interface::SessionError;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use drasi_query_cypher::CypherParser;
use tokio::sync::Notify;

use crate::{
    evaluation::{functions::FunctionRegistry, EvaluationError},
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{
        ElementIndex, FutureQueueConsumer, IndexError, MiddlewareError, MiddlewareSetupError,
        NoOpSessionControl, PushType, SessionControl, SessionGuard, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementReference, SourceChange, SourceChangeNormalizer,
        SourceInput, SourceMiddlewareConfig,
    },
    query::QueryBuilder,
};

fn node(id: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::new([Arc::from("Raw")]),
                effective_from: 1,
            },
            properties: Default::default(),
        },
    }
}

fn builder(text: &str) -> QueryBuilder {
    QueryBuilder::new(
        text,
        Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new()))),
    )
}

#[derive(Default)]
struct SelectAfterMiddleware {
    calls: AtomicUsize,
}

impl SourceChangeNormalizer for SelectAfterMiddleware {
    fn normalize(&self, mut change: SourceChange) -> SourceChange {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let SourceChange::Insert { element } | SourceChange::Update { element } = &mut change {
            if let Element::Node { metadata, .. } = element {
                metadata.labels = Arc::new([Arc::from("Selected")]);
            }
        }
        change
    }
}

#[tokio::test]
async fn source_input_survives_all_entrypoints_and_several_nested_calls() {
    let control: Arc<dyn SessionControl> = Arc::new(NoOpSessionControl);
    let query = builder("MATCH (n:Selected) RETURN n")
        .with_session_control(control.clone())
        .build()
        .await;
    let selector = Arc::new(SelectAfterMiddleware::default());
    let input = |id| SourceInput::with_normalizer(node(id), selector.clone());

    assert_eq!(
        query
            .process_source_change_with_result_hook(input("result-hook"), |results| {
                assert_eq!(results.len(), 1);
                async { Ok(()) }
            })
            .await
            .unwrap()
            .len(),
        1
    );
    let mut root = SessionGuard::begin(control.clone()).await.unwrap();
    let first = query
        .process_source_change_in(&mut root, input("nested"))
        .await
        .unwrap();
    assert!(matches!(
        query.process_source_change(node("unrelated")).await,
        Err(EvaluationError::IndexError(error)) if error.session_error() == Some(SessionError::SessionBusy)
    ));
    let second = query
        .process_source_change_in_with_hook(&mut root, input("nested-hook"), |results| {
            assert_eq!(results.len(), 1);
            async { Ok(()) }
        })
        .await
        .unwrap();
    let receipt = root.commit_with_receipt().await.unwrap();
    assert_eq!(first.into_committed(&receipt).unwrap().len(), 1);
    assert_eq!(second.into_committed(&receipt).unwrap().len(), 1);
    assert_eq!(selector.calls.load(Ordering::Relaxed), 3);
    let mut wrong_root = SessionGuard::begin(Arc::new(NoOpSessionControl))
        .await
        .unwrap();
    assert!(matches!(
        query
            .process_source_change_in(&mut wrong_root, input("wrong-root"))
            .await,
        Err(EvaluationError::IndexError(error)) if error.session_error() == Some(SessionError::InvalidSession)
    ));
    assert_eq!(selector.calls.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn future_entrypoints_share_the_root_without_reusing_source_normalizers() {
    let control = Arc::new(NoOpSessionControl);
    let query = builder("MATCH (n:Selected) RETURN n")
        .with_session_control(control.clone())
        .build()
        .await;
    let selector = Arc::new(SelectAfterMiddleware::default());
    query
        .process_source_change_with_result_hook(
            SourceInput::with_normalizer(node("n"), selector.clone()),
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    let queue = query.future_queue();
    let reference = ElementReference::new("source", "n");
    let initial = SessionGuard::begin(control.clone()).await.unwrap();
    initial.mark_dirty().unwrap();
    for signature in 1..=4 {
        queue
            .push(
                PushType::Always,
                1,
                signature,
                &reference,
                1,
                signature * 10,
            )
            .await
            .unwrap();
    }
    initial.commit().await.unwrap();
    let mut root = SessionGuard::begin(control).await.unwrap();
    let first = query.process_due_futures_in(&mut root).await.unwrap();
    let second = query
        .process_due_futures_in_with_hook(&mut root, |result| {
            assert_eq!(result.as_ref().unwrap().source_id.as_ref(), "source");
            async { Ok(()) }
        })
        .await
        .unwrap();
    let receipt = root.commit_with_receipt().await.unwrap();
    assert!(first.into_committed(&receipt).unwrap().is_some());
    assert!(second.into_committed(&receipt).unwrap().is_some());
    assert!(query
        .process_due_futures_with_hook(|result| {
            assert!(result.is_some());
            async { Ok(()) }
        })
        .await
        .unwrap()
        .is_some());
    assert!(query.process_due_futures().await.unwrap().is_some());
    assert!(query.process_due_futures().await.unwrap().is_none());
    assert_eq!(selector.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn dirty_memory_parent_rollback_fences_the_query() {
    let control = Arc::new(NoOpSessionControl);
    let query = builder("MATCH (n) RETURN n")
        .with_session_control(control.clone())
        .build()
        .await;
    let mut root = SessionGuard::begin(control).await.unwrap();
    let staged = query
        .process_source_change_in(&mut root, node("tentative"))
        .await
        .unwrap();
    drop(root);
    assert!(matches!(
        query.check_health(),
        Err(error) if error.execution_error() == Some(&QueryExecutionError::QueryRequiresRebuild)
    ));
    assert!(matches!(
        query.process_source_change(node("retry")).await,
        Err(error) if error.execution_error() == Some(&QueryExecutionError::QueryRequiresRebuild)
    ));
    let other_receipt = SessionGuard::begin(Arc::new(NoOpSessionControl))
        .await
        .unwrap()
        .commit_with_receipt()
        .await
        .unwrap();
    assert!(matches!(
        staged.into_committed(&other_receipt),
        Err(error) if error.session_error() == Some(SessionError::InvalidSession)
    ));
}

struct BlockingMiddleware {
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl SourceMiddleware for BlockingMiddleware {
    async fn process(
        &self,
        change: SourceChange,
        index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        if let SourceChange::Insert { element } = &change {
            index.set_element(element, &vec![0]).await?;
        }
        self.entered.notify_one();
        self.release.notified().await;
        Ok(vec![change])
    }
}

struct BlockingFactory(Arc<BlockingMiddleware>);

impl SourceMiddlewareFactory for BlockingFactory {
    fn name(&self) -> String {
        "blocking".into()
    }

    fn create(
        &self,
        _: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn cancellation_after_middleware_write_fences_memory() {
    let middleware = Arc::new(BlockingMiddleware {
        entered: Notify::new(),
        release: Notify::new(),
    });
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(BlockingFactory(middleware.clone())));
    let index = Arc::new(InMemoryElementIndex::new());
    let query = Arc::new(
        builder("MATCH (n) RETURN n")
            .with_element_index(index.clone())
            .with_middleware_registry(Arc::new(registry))
            .with_source_middleware(Arc::new(SourceMiddlewareConfig {
                name: Arc::from("block"),
                kind: Arc::from("blocking"),
                config: Default::default(),
            }))
            .with_source_pipeline("source", &["block".into()])
            .build()
            .await,
    );
    let worker_query = query.clone();
    let worker =
        tokio::spawn(async move { worker_query.process_source_change(node("written")).await });
    middleware.entered.notified().await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(index
        .get_element(&ElementReference::new("source", "written"))
        .await
        .unwrap()
        .is_some());
    assert!(matches!(
        query.check_health(),
        Err(error) if error.execution_error() == Some(&QueryExecutionError::QueryRequiresRebuild)
    ));
    assert!(matches!(
        query.process_source_change(node("retry")).await,
        Err(error) if error.execution_error() == Some(&QueryExecutionError::QueryRequiresRebuild)
    ));
}

#[tokio::test]
async fn cancellation_in_nested_hook_fences_memory() {
    let control = Arc::new(NoOpSessionControl);
    let query = Arc::new(
        builder("MATCH (n) RETURN n")
            .with_session_control(control.clone())
            .build()
            .await,
    );
    let entered = Arc::new(Notify::new());
    let worker_entered = entered.clone();
    let worker_query = query.clone();
    let worker = tokio::spawn(async move {
        let mut root = SessionGuard::begin(control).await.unwrap();
        worker_query
            .process_source_change_in_with_hook(&mut root, node("written"), move |results| {
                assert_eq!(results.len(), 1);
                async move {
                    worker_entered.notify_one();
                    std::future::pending::<Result<(), IndexError>>().await
                }
            })
            .await
    });
    entered.notified().await;
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(matches!(
        query.check_health(),
        Err(error) if error.execution_error() == Some(&QueryExecutionError::QueryRequiresRebuild)
    ));
}

struct FencedConsumer {
    notified: Notify,
    calls: AtomicUsize,
}

#[async_trait]
impl FutureQueueConsumer for FencedConsumer {
    async fn on_items_due(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        panic!("a fenced queue must not consume futures");
    }

    async fn on_error(&self, error: Box<dyn std::error::Error + Send + Sync>) {
        assert_eq!(
            error.downcast_ref::<IndexError>(),
            Some(&IndexError::other(SessionError::SessionFenced))
        );
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.notified.notify_one();
    }

    fn now(&self) -> u64 {
        100
    }
}

#[tokio::test]
async fn fenced_future_consumer_reports_recovery_and_stops() {
    let control = Arc::new(NoOpSessionControl);
    let query = builder("MATCH (n) RETURN n")
        .with_session_control(control.clone())
        .build()
        .await;
    let mut root = SessionGuard::begin(control).await.unwrap();
    let _staged = query
        .process_source_change_in(&mut root, node("tentative"))
        .await
        .unwrap();
    drop(root);
    let consumer = Arc::new(FencedConsumer {
        notified: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    query.set_future_consumer(consumer.clone()).await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        consumer.notified.notified().await;
        query.terminate_future_consumer().await;
    })
    .await
    .unwrap();
    assert_eq!(consumer.calls.load(Ordering::Relaxed), 1);
}
