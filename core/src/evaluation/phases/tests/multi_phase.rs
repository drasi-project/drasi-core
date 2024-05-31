use std::{collections::BTreeMap, sync::Arc};

use super::process_solution;

use crate::{
    evaluation::{
        context::PhaseEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue, ExpressionEvaluator, QueryPhaseEvaluator,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

use serde_json::json;

#[tokio::test]
async fn aggregating_phase_to_scalar_phase_add_solution() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name as key, sum(a.Value1) as my_sum
    WHERE my_sum > 2
    RETURN key, my_sum
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(result.await, vec![PhaseEvaluationContext::Noop]);

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("bar"),
              "my_sum" => json!(5.0)
            ]
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_scalar_phase_update_solution() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name as interim_key, sum(a.Value1) as interim_sum
    WHERE interim_sum > 2
    RETURN interim_key as key, interim_sum as my_sum
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(4.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(4.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1b.clone()],
            after: variablemap!["a" => node1c.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Removing {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1c.clone()],
            after: variablemap!["a" => node1d.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(7.0)
            ],
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node2.clone()],
            after: variablemap!["a" => node2a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(7.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(5.0)
            ],
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_scalar_phase_remove_solution() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name as key, sum(a.Value1) as my_sum
    WHERE my_sum > 0
    RETURN key, my_sum
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Removing {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ],
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_aggregating_phase_add_solution() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WHERE a.Value1 < 10 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    RETURN Category, avg(my_sum) as my_avg
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
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
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
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
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
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
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_aggregating_phase_update_solution() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    RETURN Category, avg(my_sum) as my_avg
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
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
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_aggregating_phase_group_switch() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    WHERE my_sum > 0
    RETURN Category, avg(my_sum) as my_avg
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
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
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![
            PhaseEvaluationContext::Aggregation {
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
            },
            PhaseEvaluationContext::Aggregation {
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
            },
        ]
    );
}

#[tokio::test]
async fn test_list_indexing_with_clause() {
    let query = drasi_query_cypher::parse(
        "
        WITH [5,1,7] AS list
        RETURN list[2]",
    )
    .unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: BTreeMap::new(),
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap![
              "expression" => json!(7)
            ]
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_aggregating_phase_group_switch_with_comments() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WITH a.Name, a.Category, sum(a.Value1) as my_sum
    // DRASI COMMENT: This is a comment
    WHERE my_sum > 0
    RETURN Category, avg(my_sum) as my_avg
    // DRASI COMMENT: This is another comment
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
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
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![
            PhaseEvaluationContext::Aggregation {
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
            },
            PhaseEvaluationContext::Aggregation {
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
            },
        ]
    );
}

#[tokio::test]
async fn sequential_aggregations1() {
    let query = drasi_query_cypher::parse(
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
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(0)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(1)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );
}

#[tokio::test]
async fn sequential_aggregations2() {
    let query = drasi_query_cypher::parse(
        "
    MATCH
        (a)
      WITH
          a.Category AS category,
          sum(a.Value1) AS s
      RETURN
          sum(s) AS total
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(0)
            ]),
            after: variablemap![
                "total" => json!(2)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(2)
            ]),
            after: variablemap![
                "total" => json!(0)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(0)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(1)
            ]),
            after: variablemap![
                "total" => json!(3)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Aggregation {
            before: Some(variablemap![
              "total" => json!(3)
            ]),
            after: variablemap![
                "total" => json!(1)
            ],
            grouping_keys: vec![],
            default_before: false,
            default_after: false,
        }]
    );
}

#[tokio::test]
async fn aggregating_phase_to_scalar_phase_add_solution_emit_remove() {
    let query = drasi_query_cypher::parse(
        "
    MATCH (a) 
    WITH a.Name as key, sum(a.Value1) as my_sum
    WHERE my_sum < 10
    RETURN key, my_sum
    ",
    )
    .unwrap();

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

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Updating {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0)
            ],
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap![
              "key" => json!("bar"),
              "my_sum" => json!(5.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node4.clone()],
        },
    )
    .await;

    println!("{:?}", result);

    assert_eq!(
        result,
        vec![PhaseEvaluationContext::Removing {
            before: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0)
            ]
        }]
    );
}
