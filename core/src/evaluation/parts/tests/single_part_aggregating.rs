use std::sync::Arc;

use super::process_solution;

use crate::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::{aggregation::ValueAccumulator, FunctionRegistry}, parts::tests::build_query, variable_value::{duration::Duration, VariableValue}, ExpressionEvaluator, QueryPartEvaluator
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
    interface::{AccumulatorIndex, ResultKey, ResultOwner},
};

use chrono::{Duration as ChronoDuration, NaiveDateTime, NaiveTime};
use serde_json::json;

#[tokio::test]
async fn aggregating_query_add_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 < 10 RETURN a.Name as key, sum(a.Value1) as my_sum, min(a.Value1) as my_min");

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
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(0.0),
              "my_min" => VariableValue::Null
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0),
              "my_min" => json!(1.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0),
              "my_min" => json!(1.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0),
              "my_min" => json!(1.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("bar"),
              "my_sum" => json!(0.0),
              "my_min" => json!(null)
            ]),
            after: variablemap![
              "key" => json!("bar"),
              "my_sum" => json!(5.0),
              "my_min" => json!(5.0)
            ]
        }]
    );
}

#[tokio::test]
async fn aggregating_query_update_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 < 10 RETURN a.Name as key, sum(a.Value1) as my_sum, min(a.Value1) as my_min");

    let node1 = json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    });

    let node1a = json!({
      "id": 1,
      "Value1": 2,
      "Name": "foo"
    });

    let node1b = json!({
      "id": 1,
      "Value1": 7,
      "Name": "foo"
    });

    let node1c = json!({
      "id": 1,
      "Value1": 17,
      "Name": "foo"
    });

    let node1d = json!({
      "id": 1,
      "Value1": 3,
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
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: false,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0),
              "my_min" => json!(1.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(4.0),
              "my_min" => json!(2.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1a.clone()],
            after: variablemap!["a" => node1b.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: false,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(4.0),
              "my_min" => json!(2.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(9.0),
              "my_min" => json!(2.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1b.clone()],
            after: variablemap!["a" => node1c.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: false,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(9.0),
              "my_min" => json!(2.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(2.0),
              "my_min" => json!(2.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1c.clone()],
            after: variablemap!["a" => node1d.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: false,
            default_after: false,
            before: None,
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(5.0),
              "my_min" => json!(2.0)
            ]
        }]
    );
}

#[tokio::test]
async fn aggregating_query_remove_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 < 10 RETURN a.Name as key, sum(a.Value1) as my_sum, min(a.Value1) as my_min");

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
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: false,
            default_after: true,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0),
              "my_min" => json!(1.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0),
              "my_min" => json!(1.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(1.0),
              "my_min" => json!(1.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0),
              "my_min" => json!(1.0)
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["key".to_string()],
            default_before: false,
            default_after: true,
            before: Some(variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(3.0),
              "my_min" => json!(1.0)
            ]),
            after: variablemap![
              "key" => json!("foo"),
              "my_sum" => json!(2.0),
              "my_min" => json!(2.0)
            ]
        }]
    );
}

#[tokio::test]
async fn group_switch() {
    let query = build_query("MATCH (a) RETURN a.Name as key, sum(a.Value1) as my_sum");

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
      "Value1": 1,
      "Name": "bar"
    });

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![
            QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["key".to_string()],
                default_before: false,
                default_after: false,
                before: Some(variablemap![
                  "key" => json!("bar"),
                  "my_sum" => json!(5.0)
                ]),
                after: variablemap![
                  "key" => json!("bar"),
                  "my_sum" => json!(6.0)
                ]
            },
            QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["key".to_string()],
                default_before: false,
                default_after: false,
                before: Some(variablemap![
                  "key" => json!("foo"),
                  "my_sum" => json!(3.0)
                ]),
                after: variablemap![
                  "key" => json!("foo"),
                  "my_sum" => json!(2.0)
                ]
            }
        ]
    );

    let foo_accumulator = match ari
        .get(
            &ResultKey::GroupBy(Arc::new(vec![VariableValue::from(json!("foo"))])),
            &ResultOwner::Function(32),
        )
        .await
        .unwrap()
        .unwrap()
    {
        ValueAccumulator::Sum { value } => value,
        _ => panic!(),
    };

    let bar_accumulator = match ari
        .get(
            &ResultKey::GroupBy(Arc::new(vec![VariableValue::from(json!("bar"))])),
            &ResultOwner::Function(32),
        )
        .await
        .unwrap()
        .unwrap()
    {
        ValueAccumulator::Sum { value } => value,
        _ => panic!(),
    };

    assert_eq!(foo_accumulator, 2.0);
    assert_eq!(bar_accumulator, 6.0);
}

#[tokio::test]
async fn group_switch_complex_accumulator() {
    let query = build_query("MATCH (a) RETURN a.Name as key, avg(a.Value1) as my_avg");

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
      "Value1": 1,
      "Name": "bar"
    });

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node3.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node1a.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![
            QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["key".to_string()],
                default_before: false,
                default_after: false,
                before: Some(variablemap![
                  "key" => json!("bar"),
                  "my_avg" => json!(5.0)
                ]),
                after: variablemap![
                  "key" => json!("bar"),
                  "my_avg" => json!(3.0)
                ]
            },
            QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["key".to_string()],
                default_before: false,
                default_after: false,
                before: Some(variablemap![
                  "key" => json!("foo"),
                  "my_avg" => json!(1.5)
                ]),
                after: variablemap![
                  "key" => json!("foo"),
                  "my_avg" => json!(2.0)
                ]
            }
        ]
    );

    let foo_accumulator = match ari
        .get(
            &ResultKey::GroupBy(Arc::new(vec![VariableValue::from(json!("foo"))])),
            &ResultOwner::Function(32),
        )
        .await
        .unwrap()
        .unwrap()
    {
        ValueAccumulator::Avg { sum, count } => (sum, count),
        _ => panic!(),
    };

    let bar_accumulator = match ari
        .get(
            &ResultKey::GroupBy(Arc::new(vec![VariableValue::from(json!("bar"))])),
            &ResultOwner::Function(32),
        )
        .await
        .unwrap()
        .unwrap()
    {
        ValueAccumulator::Avg { sum, count } => (sum, count),
        _ => panic!(),
    };

    assert_eq!(foo_accumulator, (2.0, 1));
    assert_eq!(bar_accumulator, (6.0, 2));
}

#[tokio::test]
async fn test_aggregating_function_sum_duration() {
    let query = build_query("MATCH (a) RETURN sum(a.Duration) as my_sum");

    let mut node1 = VariableValue::from(json!({
      "id": 1,
    }));

    let node1 = node1.as_object_mut().unwrap();
    node1.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(ChronoDuration::seconds(1), 0, 0)),
    );

    let mut node2 = VariableValue::from(json!({
      "id": 2,
    }));

    let node2 = node2.as_object_mut().unwrap();
    node2.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::hours(2) + ChronoDuration::minutes(1) + ChronoDuration::milliseconds(2),
            0,
            0,
        )),
    );

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "my_sum" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(0),0,0))
            ]),
            after: variablemap![
              "my_sum" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "my_sum" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]),
            after: variablemap![
              "my_sum" => VariableValue::Duration(Duration::new(ChronoDuration::hours(2) + ChronoDuration::minutes(1)+ ChronoDuration::seconds(1) + ChronoDuration::milliseconds(2),0,0))
            ]
        }]
    );
}

#[tokio::test]
async fn test_aggregating_function_max_duration() {
    let query = build_query("MATCH (a) RETURN max(a.Duration) as max_result");
    let mut node1 = VariableValue::from(json!({
        "id": 1,
    }));

    let node1 = node1.as_object_mut().unwrap();
    node1.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(ChronoDuration::seconds(1), 0, 0)),
    );

    let mut node2 = VariableValue::from(json!({
        "id": 2,
    }));

    let node2 = node2.as_object_mut().unwrap();
    node2.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::hours(2) + ChronoDuration::minutes(1) + ChronoDuration::milliseconds(2),
            0,
            0,
        )),
    );

    let mut node3 = VariableValue::from(json!({
        "id": 3,
    }));

    let node3 = node3.as_object_mut().unwrap();
    node3.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::days(1) + ChronoDuration::milliseconds(2),
            0,
            0,
        )),
    );

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "max_result" => json!(null)
            ]),
            after: variablemap![
              "max_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "max_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]),
            after: variablemap![
              "max_result" => VariableValue::Duration(Duration::new(ChronoDuration::hours(2) + ChronoDuration::minutes(1)+ ChronoDuration::milliseconds(2),0,0))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "max_result" => VariableValue::Duration(Duration::new(ChronoDuration::hours(2) + ChronoDuration::minutes(1)+ ChronoDuration::milliseconds(2),0,0))
            ]),
            after: variablemap![
              "max_result" => VariableValue::Duration(Duration::new(ChronoDuration::hours(2) + ChronoDuration::minutes(1)+ ChronoDuration::milliseconds(2),0,0))
            ]
        }]
    );
}

#[tokio::test]
async fn test_aggregating_function_max_temporal_instant() {
    let query = build_query(
        "MATCH (a) RETURN max(a.Date) as max_date, max(a.LocalDateTime) as max_localdatetime",
    );
    let mut node1 = VariableValue::from(json!({
        "id": 1,
    }));

    let node1 = node1.as_object_mut().unwrap();
    node1.insert(
        "Date".to_string(),
        VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 1).unwrap()),
    );
    node1.insert(
        "LocalDateTime".to_string(),
        VariableValue::LocalDateTime(chrono::NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2019, 6, 6).unwrap(),
            chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap(),
        )),
    );

    let mut node2 = VariableValue::from(json!({
        "id": 2,
    }));

    let node2 = node2.as_object_mut().unwrap();
    node2.insert(
        "Date".to_string(),
        VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 2).unwrap()),
    );
    node2.insert(
        "LocalDateTime".to_string(),
        VariableValue::LocalDateTime(chrono::NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2019, 6, 7).unwrap(),
            chrono::NaiveTime::from_hms_opt(2, 2, 2).unwrap(),
        )),
    );

    let mut node3 = VariableValue::from(json!({
        "id": 3,
    }));

    let node3 = node3.as_object_mut().unwrap();
    node3.insert(
        "Date".to_string(),
        VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 3).unwrap()),
    );
    node3.insert(
        "LocalDateTime".to_string(),
        VariableValue::LocalDateTime(chrono::NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2019, 6, 7).unwrap(),
            chrono::NaiveTime::from_hms_opt(11, 12, 23).unwrap(),
        )),
    );

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "max_date" => json!(null),
              "max_localdatetime" => json!(null)
            ]),
            after: variablemap![
                "max_date" => VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 1).unwrap()),
                "max_localdatetime" => VariableValue::LocalDateTime(chrono::NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 6).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
                "max_date" => VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 1).unwrap()),
                "max_localdatetime" => VariableValue::LocalDateTime(chrono::NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 6).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]),
            after: variablemap![
                "max_date" => VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 2).unwrap()),
                "max_localdatetime" => VariableValue::LocalDateTime(chrono::NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 7).unwrap(), chrono::NaiveTime::from_hms_opt(2, 2, 2).unwrap()))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
                "max_date" => VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 2).unwrap()),
                "max_localdatetime" => VariableValue::LocalDateTime(chrono::NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 7).unwrap(), chrono::NaiveTime::from_hms_opt(2, 2, 2).unwrap()))
            ]),
            after: variablemap![
                "max_date" => VariableValue::Date(chrono::NaiveDate::from_ymd_opt(2021, 1, 2).unwrap()),
                "max_localdatetime" => VariableValue::LocalDateTime(chrono::NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 7).unwrap(), chrono::NaiveTime::from_hms_opt(2, 2, 2).unwrap()))
            ]
        }]
    );
}

#[tokio::test]
async fn test_aggregating_function_min_temporal_duration() {
    let query = build_query("MATCH (a) RETURN min(a.Duration) as min_result");
    let mut node1 = VariableValue::from(json!({
        "id": 1,
    }));

    let node1 = node1.as_object_mut().unwrap();
    node1.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(ChronoDuration::seconds(1), 0, 0)),
    );

    let mut node2 = VariableValue::from(json!({
        "id": 2,
    }));

    let node2 = node2.as_object_mut().unwrap();
    node2.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::hours(2) + ChronoDuration::minutes(1) + ChronoDuration::milliseconds(2),
            0,
            0,
        )),
    );

    let mut node3 = VariableValue::from(json!({
        "id": 3,
    }));

    let node3 = node3.as_object_mut().unwrap();
    node3.insert(
        "Duration".to_string(),
        VariableValue::Duration(Duration::new(
            ChronoDuration::days(1) + ChronoDuration::milliseconds(2),
            0,
            0,
        )),
    );

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "min_result" => json!(null)
            ]),
            after: variablemap![
              "min_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node2.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "min_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]),
            after: variablemap![
              "min_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
              "min_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]),
            after: variablemap![
              "min_result" => VariableValue::Duration(Duration::new(ChronoDuration::seconds(1),0,0))
            ]
        }]
    );
}

#[tokio::test]
async fn test_aggregating_function_min_temporal_instant() {
    let query = build_query(
        "MATCH (a) RETURN min(a.LocalTime) as min_time, min(a.LocalDateTime) as min_localdatetime",
    );
    let mut node1 = VariableValue::from(json!({
        "id": 1,
    }));

    let node1 = node1.as_object_mut().unwrap();
    node1.insert(
        "LocalTime".to_string(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(21, 1, 1).unwrap()),
    );
    node1.insert(
        "LocalDateTime".to_string(),
        VariableValue::LocalDateTime(NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2019, 6, 6).unwrap(),
            chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap(),
        )),
    );

    let mut node2 = VariableValue::from(json!({
        "id": 2,
    }));
    let node2 = node2.as_object_mut().unwrap();
    node2.insert(
        "LocalTime".to_string(),
        VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(11, 32, 23, 123).unwrap()),
    );
    node2.insert(
        "LocalDateTime".to_string(),
        VariableValue::LocalDateTime(NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2019, 6, 1).unwrap(),
            chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap(),
        )),
    );

    let mut node3 = VariableValue::from(json!({
        "id": 3,
    }));

    let node3 = node3.as_object_mut().unwrap();
    node3.insert(
        "LocalTime".to_string(),
        VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(11, 32, 23, 111).unwrap()),
    );
    node3.insert(
        "LocalDateTime".to_string(),
        VariableValue::LocalDateTime(NaiveDateTime::new(
            chrono::NaiveDate::from_ymd_opt(2019, 6, 1).unwrap(),
            chrono::NaiveTime::from_hms_opt(1, 19, 1).unwrap(),
        )),
    );

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPartEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
                "min_time" => json!(null),
                "min_localdatetime" => json!(null)
            ]),
            after: variablemap![
                "min_time" => VariableValue::LocalTime(NaiveTime::from_hms_opt(21, 1, 1).unwrap()),
                "min_localdatetime" => VariableValue::LocalDateTime(NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 6).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap![
                "a" => node2.clone()
            ],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
                "min_time" => VariableValue::LocalTime(NaiveTime::from_hms_opt(21, 1, 1).unwrap()),
                "min_localdatetime" => VariableValue::LocalDateTime(NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 6).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]),
            after: variablemap![
                "min_time" => VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(11,32,23,123).unwrap()),
                "min_localdatetime" => VariableValue::LocalDateTime(NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 1).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        QueryPartEvaluationContext::Adding {
            after: variablemap![
                "a" => node1.clone()
            ],
        },
    )
    .await;

    assert_eq!(
        result,
        vec![QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap![
                "min_time" => VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(11,32,23,123).unwrap()),
                "min_localdatetime" => VariableValue::LocalDateTime(NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 1).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]),
            after: variablemap![
                "min_time" => VariableValue::LocalTime(NaiveTime::from_hms_milli_opt(11,32,23,123).unwrap()),
                "min_localdatetime" => VariableValue::LocalDateTime(NaiveDateTime::new(chrono::NaiveDate::from_ymd_opt(2019, 6, 1).unwrap(), chrono::NaiveTime::from_hms_opt(1, 1, 1).unwrap()))
            ]
        }]
    );
}
