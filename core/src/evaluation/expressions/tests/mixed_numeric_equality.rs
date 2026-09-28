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

#[tokio::test]
async fn mixed_numeric_equality_is_exact_and_inequality_is_complementary() {
    let evaluator = ExpressionEvaluator::new(
        Arc::new(FunctionRegistry::new()),
        Arc::new(InMemoryResultIndex::new()),
    );
    for (integer, float, equal) in [
        (json!(0), json!(-0.0), true),
        (json!(8), json!(8.0), true),
        (json!(-8), json!(-8.0), true),
        (json!(8), json!(8.5), false),
        (
            json!(9_007_199_254_740_992u64),
            json!(9_007_199_254_740_992.0),
            true,
        ),
        (
            json!(9_007_199_254_740_993u64),
            json!(9_007_199_254_740_992.0),
            false,
        ),
        (json!(i64::MIN), json!(i64::MIN as f64), true),
        (json!(u64::MAX), json!(u64::MAX as f64), false),
    ] {
        let integer = if let Some(value) = integer.as_i64() {
            VariableValue::Integer(value.into())
        } else {
            VariableValue::Integer(integer.as_u64().expect("integer input").into())
        };
        let float = VariableValue::from(float);
        let variables = QueryVariables::from([
            ("integer".into(), integer.clone()),
            ("float".into(), float.clone()),
        ]);
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        for (text, expected) in [
            ("integer = float", equal),
            ("float = integer", equal),
            ("integer <> float", !equal),
            ("float <> integer", !equal),
        ] {
            let expression = drasi_query_cypher::parse_expression(text).expect("comparison");
            assert_eq!(
                evaluator
                    .evaluate_expression(&context, &expression)
                    .await
                    .expect(text),
                VariableValue::Bool(expected),
                "{integer:?} / {float:?}: {text}"
            );
        }
        assert_eq!(
            integer == float,
            equal,
            "collection value equality must agree with scalar equality"
        );
    }
}
