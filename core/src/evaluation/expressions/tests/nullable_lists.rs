// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;

use crate::{
    evaluation::{
        context::QueryVariables,
        functions::{Function, FunctionRegistry, Reduce, Size},
        variable_value::VariableValue,
        EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock,
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

async fn evaluate(text: &str) -> Result<VariableValue, EvaluationError> {
    let expression = drasi_query_cypher::parse_expression(text).expect("expression");
    let functions = Arc::new(FunctionRegistry::new());
    functions.register_function("size", Function::Scalar(Arc::new(Size {})));
    functions.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));
    let evaluator = ExpressionEvaluator::new(functions, Arc::new(InMemoryResultIndex::new()));
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    evaluator.evaluate_expression(&context, &expression).await
}

#[tokio::test]
async fn list_comprehensions_propagate_null_without_evaluating_the_body() {
    for expression in [
        "[x IN null | x]",
        "[x IN null WHERE x > 0 | x + 1]",
        "[x IN null WHERE x.value = 1]",
        "[x IN null | 1 / 0]",
        "size([x IN null WHERE x = 1])",
    ] {
        assert_eq!(
            evaluate(expression).await.expect(expression),
            VariableValue::Null
        );
    }
}

#[tokio::test]
async fn list_comprehensions_preserve_empty_lists_and_reject_non_lists() {
    assert_eq!(
        evaluate("[x IN [] | 1 / 0]").await.expect("empty list"),
        VariableValue::List(Vec::new())
    );
    assert_eq!(
        evaluate("[x IN [null] | x]").await.expect("null item"),
        VariableValue::List(vec![VariableValue::Null])
    );
    assert!(
        matches!(evaluate("[x IN 7 | x]").await, Err(EvaluationError::InvalidType { expected }) if expected == "List")
    );
}

#[tokio::test]
async fn reduce_distinguishes_a_null_list_from_an_empty_list() {
    assert_eq!(
        evaluate("reduce(total = 7, x IN null | total + x)")
            .await
            .expect("null reduce"),
        VariableValue::Null
    );
    assert_eq!(
        evaluate("reduce(total = 7, x IN [] | total + x)")
            .await
            .expect("empty reduce"),
        VariableValue::Integer(7.into())
    );
}
