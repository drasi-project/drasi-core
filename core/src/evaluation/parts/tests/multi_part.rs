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

use std::{collections::BTreeMap, sync::Arc};

use super::{process_solution, IGNORED_ROW_SIGNATURE};

use crate::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::{Avg, Count, Function, FunctionRegistry, Sum},
        parts::tests::build_query,
        variable_value::VariableValue,
        ExpressionEvaluator, QueryPartEvaluator,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

use serde_json::json;

fn create_multipart_test_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());
    registry.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
    registry.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
    registry.register_function("count", Function::Aggregating(Arc::new(Count {})));
    registry
}

#[tokio::test]
async fn aggregating_part_to_scalar_part_add_solution() {
    let query = build_query(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name as key, sum(a.Value1) as my_sum
    WHERE my_sum > 2
    RETURN key, my_sum
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(result.await, vec![QueryPartEvaluationContext::Noop]);

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("bar"),
              "my_sum" => json!(5.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_scalar_part_update_solution() {
    let query = build_query(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name as interim_key, sum(a.Value1) as interim_sum
    WHERE interim_sum > 2
    RETURN interim_key as key, interim_sum as my_sum
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar"
    });

    let node1a = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo"
    });

    let node1b = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    });

    let node1c = json!({
      "id": 1,
      "Value1": 0,
      "Name": "foo"
    });

    let node1d = json!({
      "id": 1,
      "Value1": 5,
      "Name": "foo"
    });

    let node2a = json!({
      "id": 2,
      "Value1": 10,
      "Name": "foo"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(4.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(4.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1b.clone()],
            after: variablemap!["a" => node1c.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Removing {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1c.clone()],
            after: variablemap!["a" => node1d.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(7.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node2.clone()],
            after: variablemap!["a" => node2a.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(7.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(5.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_scalar_part_remove_solution() {
    let query = build_query(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name as key, sum(a.Value1) as my_sum
    WHERE my_sum > 0
    RETURN key, my_sum
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Removing {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_aggregating_part_add_solution() {
    let query = build_query(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name, sum(a.Value1) as my_sum, a.Category
    RETURN Category, avg(my_sum) as my_avg
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo",
      "Category": "A"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo",
      "Category": "B"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar",
      "Category": "A"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(0.0)
            ]),
            after: variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(1.0)
            ],
            grouping_keys: vec!["Category".to_string()],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "Category" => json!("B"),
              "my_avg" => json!(0.0)
            ]),
            after: variablemap![
              "Category" => json!("B"),
              "my_avg" => json!(2.0)
            ],
            grouping_keys: vec!["Category".to_string()],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(1.0)
            ]),
            after: variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.0)
            ],
            grouping_keys: vec!["Category".to_string()],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_aggregating_part_update_solution() {
    let query = build_query(
        "
    MATCH (a) 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    RETURN Category, avg(my_sum) as my_avg
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo",
      "Category": "A"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo",
      "Category": "B"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar",
      "Category": "A"
    });

    let node1a = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo",
      "Category": "A"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.0)
            ]),
            after: variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.5)
            ],
            grouping_keys: vec!["Category".to_string()],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_aggregating_part_group_switch() {
    let query = build_query(
        "
    MATCH (a) 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    WHERE my_sum > 0
    RETURN Category, avg(my_sum) as my_avg
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo",
      "Category": "A"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo",
      "Category": "B"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar",
      "Category": "A"
    });

    let node1a = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo",
      "Category": "A"
    });

    let node1b = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo",
      "Category": "B"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.0)
            ]),
            after: variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.5)
            ],
            grouping_keys: vec!["Category".to_string()],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![
            QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap![
                  "Category" => json!("B"),
                  "my_avg" => json!(2.0)
                ]),
                after: variablemap![
                  "Category" => json!("B"),
                  "my_avg" => json!(4.0)
                ],
                grouping_keys: vec!["Category".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            },
            QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap![
                  "Category" => json!("A"),
                  "my_avg" => json!(3.5)
                ]),
                after: variablemap![
                  "Category" => json!("A"),
                  "my_avg" => json!(5.0)
                ],
                grouping_keys: vec!["Category".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            },
        ]
    );
}

#[tokio::test]
async fn test_list_indexing_with_clause() {
    let query = build_query(
        "
        WITH [5,1,7] AS list
        RETURN list[2]",
    );

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: BTreeMap::new(),
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap![
              "expression" => json!(7)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_aggregating_part_group_switch_with_comments() {
    let query = build_query(
        "
    MATCH (a) 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    // DRASI COMMENT: This is a comment
    WHERE my_sum > 0
    RETURN Category, avg(my_sum) as my_avg
    // DRASI COMMENT: This is another comment
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo",
      "Category": "A"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo",
      "Category": "B"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar",
      "Category": "A"
    });

    let node1a = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo",
      "Category": "A"
    });

    let node1b = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo",
      "Category": "B"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.0)
            ]),
            after: variablemap![
              "Category" => json!("A"),
              "my_avg" => json!(3.5)
            ],
            grouping_keys: vec!["Category".to_string()],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![
            QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap![
                  "Category" => json!("B"),
                  "my_avg" => json!(2.0)
                ]),
                after: variablemap![
                  "Category" => json!("B"),
                  "my_avg" => json!(4.0)
                ],
                grouping_keys: vec!["Category".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            },
            QueryPartEvaluationContext::Aggregation {
                before: Some(variablemap![
                  "Category" => json!("A"),
                  "my_avg" => json!(3.5)
                ]),
                after: variablemap![
                  "Category" => json!("A"),
                  "my_avg" => json!(5.0)
                ],
                grouping_keys: vec!["Category".to_string()],
                default_before: false,
                default_after: false,
                row_signature: IGNORED_ROW_SIGNATURE,
            },
        ]
    );
}

#[tokio::test]
async fn sequential_aggregations1() {
    let query = build_query(
        "
    MATCH
        (a)
      WITH
          a.Category AS category,
          count(a) AS count
      RETURN
          count(CASE count 
                    WHEN 0 THEN NULL 
                    ELSE count 
                END
            ) AS total
    ",
    );

    let node1 = json!({
      "id": 1,
      "Category": "A"
    });

    let node2 = json!({
      "id": 2,
      "Category": "B"
    });

    let node3 = json!({
        "id": 3,
        "Category": "B"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(0)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(1)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn sequential_aggregations2() {
    let query = build_query(
        "
    MATCH
        (a)
      WITH
          a.Category AS category,
          sum(a.Value1) AS s
      RETURN
          sum(s) AS total
    ",
    );

    let node1 = json!({
      "id": 1,
      "Category": "A",
      "Value1": 2
    });

    let node2 = json!({
      "id": 2,
      "Category": "B",
      "Value1": -2
    });

    let node3 = json!({
        "id": 3,
        "Category": "B",
        "Value1": 1
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(0)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(0)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(0)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(1)
            ]),
            after: variablemap![
                "total" => json!(3)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(3)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}

#[tokio::test]
async fn aggregating_part_to_scalar_part_add_solution_emit_remove() {
    let query = build_query(
        "
    MATCH (a) 
    WITH a.Name as key, sum(a.Value1) as my_sum
    WHERE my_sum < 10
    RETURN key, my_sum
    ",
    );

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    });

    let node2 = json!({
      "id": 2,
      "Value1": 2,
      "Name": "foo"
    });

    let node3 = json!({
      "id": 3,
      "Value1": 5,
      "Name": "bar"
    });

    let node4 = json!({
      "id": 4,
      "Value1": 9,
      "Name": "foo"
    });

    let function_registry = create_multipart_test_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(expr_evaluator.clone(), ari.clone()));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("bar"),
              "my_sum" => json!(5.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node4.clone()],
            row_signature: IGNORED_ROW_SIGNATURE,
        },
    )
    .await;

    println!("{result:?}");

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Removing {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            row_signature: IGNORED_ROW_SIGNATURE,
        }]
    );
}
