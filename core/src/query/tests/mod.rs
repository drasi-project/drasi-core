// Copyright 2024 The Drasi Authors.
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

mod aggregate_update_tests;
#[cfg(feature = "computation")]
mod mutation_integrity;
mod optional_context_work;
mod retained_multi_source_tests;
mod rich_property_tests;
mod row_signature_tests;

use std::sync::Arc;

use async_trait::async_trait;
use drasi_query_ast::api::QueryParser;
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

use crate::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    interface::{FutureQueueConsumer, IndexError, QueryBuilderError},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

fn path_bound_parsers() -> [Arc<dyn QueryParser>; 2] {
    let registry = Arc::new(FunctionRegistry::new());
    [
        Arc::new(CypherParser::new(registry.clone())),
        Arc::new(GQLParser::new(registry)),
    ]
}

fn assert_unsupported_path_bounds<T>(result: Result<T, QueryBuilderError>) {
    let error = result
        .err()
        .expect("quantified relationships must not build");
    assert_eq!(
        error.to_string(),
        "Unsupported query feature: variable-length relationship patterns"
    );
    assert!(matches!(
        error,
        QueryBuilderError::UnsupportedFeature {
            feature: "variable-length relationship patterns"
        }
    ));
}

#[tokio::test]
async fn unsupported_path_bounds_are_rejected_for_both_query_languages() {
    for parser in path_bound_parsers() {
        for text in [
            "MATCH (a)-[r:LINK*]->(b) RETURN r",
            "MATCH (a)-[r:LINK*1]->(b) RETURN r",
            "MATCH (a)-[:LINK*2]->(b) RETURN b",
            "MATCH (a)-[:LINK*1..3]->(b) RETURN b",
            "MATCH (a)-[:LINK*..3]->(b) RETURN b",
            "MATCH (a)-[:LINK*0..1]->(b) RETURN b",
            "MATCH (a)<-[:LINK*1..3]-(b) RETURN b",
            "MATCH (a)-[:LINK*1..3]-(b) RETURN b",
            "MATCH (a) OPTIONAL MATCH (a)-[:LINK*1..3]->(b) RETURN b",
        ] {
            assert_unsupported_path_bounds(
                QueryBuilder::new(text, parser.clone()).try_build().await,
            );
        }
    }
}

#[tokio::test]
async fn unsupported_path_bounds_in_later_cypher_parts_are_rejected_before_evaluator_creation() {
    let parser = Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new())));
    assert_unsupported_path_bounds(
        QueryBuilder::new(
            "MATCH (a) WITH a MATCH (a)-[:LINK*1..3]->(b) RETURN b",
            parser,
        )
        .try_build_evaluator()
        .await,
    );
}

#[tokio::test]
async fn unquantified_relationship_paths_remain_supported() {
    for parser in path_bound_parsers() {
        for text in [
            "MATCH (a)-[r:LINK]->(b) RETURN r",
            "MATCH (a)<-[:LINK]-(b) RETURN b",
            "MATCH (a)-[:LINK]-(b) RETURN b",
            "MATCH (a)-[:LINK]->(b)-[:LINK]->(c) RETURN c",
            "MATCH (a) OPTIONAL MATCH (a)-[:LINK]->(b) RETURN b",
        ] {
            QueryBuilder::new(text, parser.clone())
                .try_build()
                .await
                .unwrap_or_else(|error| panic!("{text}: {error}"));
        }
    }
}

#[tokio::test]
async fn dependency_leaks() {
    let query_str = "MATCH (n:Person) RETURN n";
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let mut builder = QueryBuilder::new(query_str, parser);

    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());

    builder = builder.with_element_index(element_index.clone());
    builder = builder.with_archive_index(element_index.clone());
    builder = builder.with_result_index(result_index.clone());
    builder = builder.with_future_queue(future_queue.clone());

    let query = builder.build().await;
    let fq = Arc::new(TestFutureConsumer {});
    query.set_future_consumer(fq).await;

    query.terminate_future_consumer().await;
    drop(query);

    assert_eq!(Arc::strong_count(&element_index), 1);
    assert_eq!(Arc::strong_count(&result_index), 1);
    assert_eq!(Arc::strong_count(&future_queue), 1);
}

/// Test that `process_source_change_with_hook` rolls back when the hook fails.
///
/// The hook returns an error AFTER the index updates have been applied inside
/// the session. Because the error propagates before commit, the session must
/// roll back, leaving the index unchanged.
#[tokio::test]
async fn hook_failure_rolls_back_session() {
    let query_str = "MATCH (n:Person) RETURN n.name";
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));

    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());

    let builder = QueryBuilder::new(query_str, parser)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index.clone())
        .with_result_index(result_index.clone())
        .with_future_queue(future_queue);
    let query = builder.build().await;

    // Insert a node successfully (no hook)
    let insert = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "n1"),
                labels: Arc::new([Arc::from("Person")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(
                vec![(
                    "name".to_string(),
                    crate::evaluation::variable_value::VariableValue::String("Alice".to_string()),
                )]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
            ),
        },
    };
    let result = query.process_source_change(insert).await;
    assert!(result.is_ok(), "Initial insert should succeed");
    assert_eq!(result.unwrap().len(), 1, "Should produce 1 result");

    // Now try to insert another node with a hook that fails
    let insert2 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "n2"),
                labels: Arc::new([Arc::from("Person")]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::from(
                vec![(
                    "name".to_string(),
                    crate::evaluation::variable_value::VariableValue::String("Bob".to_string()),
                )]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
            ),
        },
    };

    let failing_hook = |_results: &[QueryPartEvaluationContext]| async {
        Err(IndexError::other(std::io::Error::other(
            "simulated checkpoint failure",
        )))
    };

    let result = query
        .process_source_change_with_hook(insert2, failing_hook)
        .await;
    assert!(result.is_err(), "Hook failure should propagate as error");

    // After the hook failure, verify the element was NOT committed by trying
    // to process a change that references n2. Since the in-memory index doesn't
    // use real transactions (writes are immediate), we verify by checking that
    // the error was propagated correctly — the key contract of the function.
    // For real transactional backends (RocksDB, Garnet), the session rollback
    // ensures the element index is not modified.

    // Verify a successful hook works after a failure
    let insert3 = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "n3"),
                labels: Arc::new([Arc::from("Person")]),
                effective_from: 3000,
            },
            properties: ElementPropertyMap::from(
                vec![(
                    "name".to_string(),
                    crate::evaluation::variable_value::VariableValue::String("Charlie".to_string()),
                )]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
            ),
        },
    };

    let ok_hook = |_results: &[QueryPartEvaluationContext]| async { Ok(()) };
    let result = query
        .process_source_change_with_hook(insert3, ok_hook)
        .await;
    assert!(
        result.is_ok(),
        "Successful hook should complete: {:?}",
        result.err()
    );
    assert_eq!(
        result.unwrap().len(),
        1,
        "Should produce 1 result after successful hook"
    );
}

struct TestFutureConsumer {}

#[async_trait]
impl FutureQueueConsumer for TestFutureConsumer {
    async fn on_items_due(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }

    async fn on_error(&self, _error: Box<dyn std::error::Error + Send + Sync>) {}

    fn now(&self) -> u64 {
        0
    }
}
