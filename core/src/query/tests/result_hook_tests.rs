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

use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use async_trait::async_trait;
use drasi_query_ast::ast;
use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{
            aggregation::RegisterAggregationFunctions, Function, FunctionRegistry, ScalarFunction,
        },
        variable_value::VariableValue,
        EvaluationError, ExpressionEvaluationContext, FunctionError, FunctionEvaluationError,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
    },
    interface::{
        ElementIndex, FutureQueue, IndexError, MiddlewareError, MiddlewareSetupError,
        SessionControl, SourceMiddleware, SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
    query::{ContinuousQuery, QueryBuilder},
};

struct AtomicTestSessionControl;

#[async_trait]
impl SessionControl for AtomicTestSessionControl {
    fn supports_atomic_sessions(&self) -> bool {
        true
    }

    async fn begin(&self) -> Result<(), IndexError> {
        Ok(())
    }

    async fn commit(&self) -> Result<(), IndexError> {
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        Ok(())
    }
}

async fn build_query(query: &str, functions: Arc<FunctionRegistry>) -> ContinuousQuery {
    let parser = Arc::new(CypherParser::new(functions.clone()));
    QueryBuilder::new(query, parser)
        .with_function_registry(functions)
        .with_session_control(Arc::new(AtomicTestSessionControl))
        .build()
        .await
}

fn person_change(id: &str, name: &str, effective_from: u64, update: bool) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", id),
            labels: Arc::new([Arc::from("Person")]),
            effective_from,
        },
        properties: ElementPropertyMap::from(json!({
            "name": name,
            "age": 30,
        })),
    };

    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn value<'a>(variables: &'a QueryVariables, key: &str) -> &'a VariableValue {
    variables
        .get(key)
        .unwrap_or_else(|| panic!("missing result variable {key}"))
}

#[tokio::test]
async fn result_hook_shares_exact_add_update_delete_and_aggregation_results() {
    let query = build_query(
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;

    let observed_pointer = Arc::new(AtomicUsize::new(0));
    let hook_pointer = observed_pointer.clone();
    let added = query
        .process_source_change_with_result_hook(
            person_change("person-1", "Alice", 1_000, false),
            move |results| async move {
                tokio::task::yield_now().await;
                hook_pointer.store(results.as_ptr() as usize, Ordering::Relaxed);
                let [QueryPartEvaluationContext::Adding { after, .. }] = results.as_ref() else {
                    panic!("expected one adding result, got {results:?}");
                };
                assert_eq!(value(after, "name"), &VariableValue::from("Alice"));
                Ok(())
            },
        )
        .await
        .expect("add should succeed");
    assert_eq!(
        observed_pointer.load(Ordering::Relaxed),
        added.as_ptr() as usize,
        "the hook and caller must share the result allocation"
    );

    let updated = query
        .process_source_change_with_result_hook(
            person_change("person-1", "Alicia", 2_000, true),
            |results| async move {
                tokio::task::yield_now().await;
                let [QueryPartEvaluationContext::Updating { before, after, .. }] = results.as_ref()
                else {
                    panic!("expected one updating result, got {results:?}");
                };
                assert_eq!(value(before, "name"), &VariableValue::from("Alice"));
                assert_eq!(value(after, "name"), &VariableValue::from("Alicia"));
                Ok(())
            },
        )
        .await
        .expect("update should succeed");
    assert!(matches!(
        updated.as_ref(),
        [QueryPartEvaluationContext::Updating { .. }]
    ));

    let removed = query
        .process_source_change_with_result_hook(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("people", "person-1"),
                    labels: Arc::new([Arc::from("Person")]),
                    effective_from: 3_000,
                },
            },
            |results| async move {
                tokio::task::yield_now().await;
                let [QueryPartEvaluationContext::Removing { before, .. }] = results.as_ref() else {
                    panic!("expected one removing result, got {results:?}");
                };
                assert_eq!(value(before, "name"), &VariableValue::from("Alicia"));
                Ok(())
            },
        )
        .await
        .expect("delete should succeed");
    assert!(matches!(
        removed.as_ref(),
        [QueryPartEvaluationContext::Removing { .. }]
    ));

    let functions = Arc::new(FunctionRegistry::new());
    functions.register_aggregation_functions();
    let aggregate_query = build_query("MATCH (n:Person) RETURN count(n) AS total", functions).await;
    let aggregated = aggregate_query
        .process_source_change_with_result_hook(
            person_change("person-2", "Grace", 4_000, false),
            |results| async move {
                tokio::task::yield_now().await;
                let [QueryPartEvaluationContext::Aggregation {
                    before,
                    after,
                    grouping_keys,
                    default_before,
                    default_after,
                    ..
                }] = results.as_ref()
                else {
                    panic!("expected one aggregation result, got {results:?}");
                };
                assert_eq!(
                    value(before.as_ref().expect("default count result"), "total"),
                    &VariableValue::Integer(0.into())
                );
                assert!(grouping_keys.is_empty());
                assert!(*default_before);
                assert!(!default_after);
                assert_eq!(value(after, "total"), &VariableValue::Integer(1.into()));
                Ok(())
            },
        )
        .await
        .expect("aggregation should succeed");
    assert!(matches!(
        aggregated.as_ref(),
        [QueryPartEvaluationContext::Aggregation { .. }]
    ));
}

#[derive(Default)]
struct RecordingSessionControl {
    calls: Mutex<Vec<&'static str>>,
}

impl RecordingSessionControl {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().expect("calls lock poisoned").clone()
    }
}

#[async_trait]
impl SessionControl for RecordingSessionControl {
    fn supports_atomic_sessions(&self) -> bool {
        true
    }

    async fn begin(&self) -> Result<(), IndexError> {
        self.calls
            .lock()
            .expect("calls lock poisoned")
            .push("begin");
        Ok(())
    }

    async fn commit(&self) -> Result<(), IndexError> {
        self.calls
            .lock()
            .expect("calls lock poisoned")
            .push("commit");
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.calls
            .lock()
            .expect("calls lock poisoned")
            .push("rollback");
        Ok(())
    }
}

struct FailingScalar;

#[async_trait]
impl ScalarFunction for FailingScalar {
    async fn call(
        &self,
        _context: &ExpressionEvaluationContext,
        expression: &ast::FunctionExpression,
        _args: Vec<VariableValue>,
    ) -> Result<VariableValue, FunctionError> {
        Err(FunctionError {
            function_name: expression.name.to_string(),
            error: FunctionEvaluationError::InvalidArgumentCount,
        })
    }
}

#[tokio::test]
async fn result_hook_is_not_called_when_evaluation_fails() {
    let functions = Arc::new(FunctionRegistry::new());
    functions.register_function("fail", Function::Scalar(Arc::new(FailingScalar)));
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let session_control = Arc::new(RecordingSessionControl::default());
    let query = QueryBuilder::new("MATCH (n:Person) RETURN fail() AS value", parser)
        .with_function_registry(functions)
        .with_session_control(session_control.clone())
        .build()
        .await;
    let hook_called = Arc::new(AtomicBool::new(false));
    let hook_called_inside = hook_called.clone();

    let result = query
        .process_source_change_with_result_hook(
            person_change("person-1", "Alice", 1_000, false),
            move |_| async move {
                hook_called_inside.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;

    assert!(result.is_err());
    assert!(!hook_called.load(Ordering::Relaxed));
    assert_eq!(session_control.calls(), vec!["begin", "rollback"]);
}

struct FanOutMiddleware;

#[async_trait]
impl SourceMiddleware for FanOutMiddleware {
    async fn process(
        &self,
        source_change: SourceChange,
        _element_index: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        let source_id = source_change.get_reference().source_id.clone();
        let effective_from = source_change.get_transaction_time();
        Ok(["first", "second"]
            .into_iter()
            .map(|name| SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source_id.as_ref(), name),
                        labels: Arc::new([Arc::from("Person")]),
                        effective_from,
                    },
                    properties: ElementPropertyMap::from(json!({ "name": name })),
                },
            })
            .collect())
    }
}

struct FanOutFactory;

impl SourceMiddlewareFactory for FanOutFactory {
    fn name(&self) -> String {
        "fan-out".to_string()
    }

    fn create(
        &self,
        _config: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(FanOutMiddleware))
    }
}

#[tokio::test]
async fn result_hook_preserves_empty_and_middleware_fan_out_results() {
    let no_result_query = build_query(
        "MATCH (n:Person) WHERE n.age > 100 RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    let empty_hook_called = Arc::new(AtomicBool::new(false));
    let empty_hook_called_inside = empty_hook_called.clone();
    let empty = no_result_query
        .process_source_change_with_result_hook(
            person_change("person-1", "Alice", 1_000, false),
            move |results| async move {
                tokio::task::yield_now().await;
                assert!(results.is_empty());
                empty_hook_called_inside.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await
        .expect("no-result evaluation should succeed");
    assert!(empty.is_empty());
    assert!(empty_hook_called.load(Ordering::Relaxed));

    let functions = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(FanOutFactory));
    let query = QueryBuilder::new("MATCH (n:Person) RETURN n.name AS name", parser)
        .with_function_registry(functions)
        .with_middleware_registry(Arc::new(registry))
        .with_source_middleware(Arc::new(SourceMiddlewareConfig::new(
            "fan-out",
            "fan-out",
            serde_json::Map::new(),
        )))
        .with_source_pipeline("people", &["fan-out".to_string()])
        .with_session_control(Arc::new(AtomicTestSessionControl))
        .build()
        .await;

    let results = query
        .process_source_change_with_result_hook(
            person_change("ignored", "ignored", 2_000, false),
            |results| async move {
                tokio::task::yield_now().await;
                let names = results
                    .iter()
                    .map(|result| match result {
                        QueryPartEvaluationContext::Adding { after, .. } => {
                            value(after, "name").to_string()
                        }
                        other => panic!("expected adding result, got {other:?}"),
                    })
                    .collect::<BTreeSet<_>>();
                assert_eq!(
                    names,
                    BTreeSet::from(["first".to_string(), "second".to_string()])
                );
                Ok(())
            },
        )
        .await
        .expect("fan-out evaluation should succeed");
    assert_eq!(results.len(), 2);
}

struct BorrowingHost {
    expected_name: String,
    hook_called: AtomicBool,
}

impl BorrowingHost {
    async fn run(
        &self,
        query: &ContinuousQuery,
        change: SourceChange,
    ) -> Result<Arc<[QueryPartEvaluationContext]>, EvaluationError> {
        query
            .process_source_change_with_result_hook(change, |results| async move {
                tokio::task::yield_now().await;
                let [QueryPartEvaluationContext::Adding { after, .. }] = results.as_ref() else {
                    panic!("expected one adding result, got {results:?}");
                };
                assert_eq!(
                    value(after, "name"),
                    &VariableValue::from(self.expected_name.as_str())
                );
                self.hook_called.store(true, Ordering::Relaxed);
                Ok(())
            })
            .await
    }
}

#[tokio::test]
async fn result_hook_can_borrow_host_state_across_await() {
    let query = build_query(
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    let host = BorrowingHost {
        expected_name: "Ada".to_string(),
        hook_called: AtomicBool::new(false),
    };

    let results = host
        .run(&query, person_change("person-1", "Ada", 1_000, false))
        .await
        .expect("borrowed host hook should compile and run");

    assert_eq!(results.len(), 1);
    assert!(host.hook_called.load(Ordering::Relaxed));
}

#[tokio::test]
async fn result_hooks_reject_non_atomic_sessions_before_mutation_or_pop() {
    let functions = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let query = QueryBuilder::new("MATCH (n:Person) RETURN n.name AS name", parser)
        .with_function_registry(functions)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index.clone())
        .build()
        .await;
    assert!(!query.supports_atomic_result_hooks());

    let source_hook_called = Arc::new(AtomicBool::new(false));
    let called = source_hook_called.clone();
    let source_result = query
        .process_source_change_with_result_hook(
            person_change("person-1", "Alice", 1_000, false),
            move |_| async move {
                called.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;
    assert!(matches!(
        source_result,
        Err(EvaluationError::IndexError(
            IndexError::AtomicSessionNotSupported
        ))
    ));
    assert!(!source_hook_called.load(Ordering::Relaxed));
    assert!(
        element_index
            .get_element(&ElementReference::new("people", "person-1"))
            .await
            .expect("read in-memory element")
            .is_none(),
        "rejection must happen before core mutation"
    );

    let functions = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());
    let future_query = QueryBuilder::new(
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        parser,
    )
    .with_function_registry(functions)
    .with_element_index(element_index.clone())
    .with_archive_index(element_index)
    .with_future_queue(future_queue.clone())
    .build()
    .await;
    let initial = future_query
        .process_source_change(person_change("future-person", "Grace", 1_000, false))
        .await
        .expect("legacy processing should queue a future");
    assert!(initial.is_empty());
    assert_eq!(
        future_queue.peek_due_time().await.expect("peek future"),
        Some(2_000)
    );

    let future_hook_called = Arc::new(AtomicBool::new(false));
    let called = future_hook_called.clone();
    let due_result = future_query
        .process_due_futures_with_result_hook(move |_| async move {
            called.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await;
    assert!(matches!(
        due_result,
        Err(EvaluationError::IndexError(
            IndexError::AtomicSessionNotSupported
        ))
    ));
    assert!(!future_hook_called.load(Ordering::Relaxed));
    assert_eq!(
        future_queue
            .peek_due_time()
            .await
            .expect("peek retained future"),
        Some(2_000),
        "rejection must happen before the future pop"
    );
}
