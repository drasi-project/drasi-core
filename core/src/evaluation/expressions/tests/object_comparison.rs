// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use crate::{
    evaluation::{
        context::QueryVariables, functions::FunctionRegistry, variable_value::VariableValue,
        ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};
use serde_json::json;

async fn evaluate(text: &str, variables: &QueryVariables) -> VariableValue {
    let expression = drasi_query_cypher::parse_expression(text).expect("expression");
    let evaluator = ExpressionEvaluator::new(
        Arc::new(FunctionRegistry::new()),
        Arc::new(InMemoryResultIndex::new()),
    );
    let context =
        ExpressionEvaluationContext::new(variables, Arc::new(InstantQueryClock::new(0, 0)));
    evaluator
        .evaluate_expression(&context, &expression)
        .await
        .expect("evaluation")
}

#[tokio::test]
async fn object_equality_and_inequality_compare_all_keys_and_nested_values() {
    for (left, right, equal) in [
        (json!({}), json!({}), true),
        (json!({"a":1,"b":2}), json!({"b":2,"a":1}), true),
        (
            json!({"nested":{"items":[1,2,null]}}),
            json!({"nested":{"items":[1.0,2,null]}}),
            true,
        ),
        (json!({"a":1}), json!({"a":2}), false),
        (json!({"a":1}), json!({"a":1,"b":2}), false),
        (json!({"a":null}), json!({}), false),
        (json!({"revision":"1"}), json!({"revision":1}), false),
        (
            json!({"nested":{"items":[1,2]}}),
            json!({"nested":{"items":[2,1]}}),
            false,
        ),
    ] {
        let variables = QueryVariables::from([
            ("left".into(), VariableValue::from(left)),
            ("right".into(), VariableValue::from(right)),
        ]);
        assert_eq!(
            evaluate("left = right", &variables).await,
            VariableValue::Bool(equal)
        );
        assert_eq!(
            evaluate("left <> right", &variables).await,
            VariableValue::Bool(!equal)
        );
    }
}

#[tokio::test]
async fn list_comprehension_matches_equal_property_maps() {
    let assignment = json!({"gpu_id":"gpu-1","workload_revision":"1","memory_mib":4096});
    let variables = QueryVariables::from([
        (
            "p".into(),
            VariableValue::from(json!({"assignments":[assignment.clone()]})),
        ),
        (
            "w".into(),
            VariableValue::from(json!({"assignment":assignment.clone()})),
        ),
    ]);
    assert_eq!(
        evaluate("[d IN p.assignments WHERE d = w.assignment]", &variables).await,
        VariableValue::List(vec![VariableValue::from(assignment)]),
    );
}
