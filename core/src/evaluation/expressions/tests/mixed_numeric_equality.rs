// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{cmp::Ordering, sync::Arc};

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

#[tokio::test]
async fn numeric_relational_operators_preserve_precision_and_unsigned_values() {
    let evaluator = ExpressionEvaluator::new(
        Arc::new(FunctionRegistry::new()),
        Arc::new(InMemoryResultIndex::new()),
    );
    let signed = |value: i64| VariableValue::Integer(value.into());
    let unsigned = |value: u64| VariableValue::Integer(value.into());
    let float = |value: f64| VariableValue::Float(value.into());
    for (left, right, expected) in [
        (signed(0), float(-0.0), Some(Ordering::Equal)),
        (signed(0), float(-0.5), Some(Ordering::Greater)),
        (signed(0), float(0.5), Some(Ordering::Less)),
        (
            unsigned(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0),
            Some(Ordering::Greater),
        ),
        (
            signed(-9_007_199_254_740_993),
            float(-9_007_199_254_740_992.0),
            Some(Ordering::Less),
        ),
        (
            signed(i64::MIN),
            float(i64::MIN as f64),
            Some(Ordering::Equal),
        ),
        (
            signed(i64::MIN + 1),
            float(i64::MIN as f64),
            Some(Ordering::Greater),
        ),
        (
            signed(i64::MAX),
            float(i64::MAX as f64),
            Some(Ordering::Less),
        ),
        (
            unsigned(1u64 << 63),
            float((1u64 << 63) as f64),
            Some(Ordering::Equal),
        ),
        (
            unsigned(u64::MAX - 1),
            unsigned(u64::MAX),
            Some(Ordering::Less),
        ),
        (
            unsigned(u64::MAX),
            float(u64::MAX as f64),
            Some(Ordering::Less),
        ),
        (unsigned(1u64 << 63), signed(-1), Some(Ordering::Greater)),
        (
            float(f64::INFINITY),
            unsigned(u64::MAX),
            Some(Ordering::Greater),
        ),
        (
            float(f64::NEG_INFINITY),
            signed(i64::MIN),
            Some(Ordering::Less),
        ),
        (float(f64::NAN), signed(0), None),
        (float(f64::NAN), float(f64::NAN), None),
    ] {
        for (left, right, expected) in [
            (left.clone(), right.clone(), expected),
            (right, left, expected.map(Ordering::reverse)),
        ] {
            let variables = QueryVariables::from([
                ("left".into(), left.clone()),
                ("right".into(), right.clone()),
            ]);
            let context = ExpressionEvaluationContext::new(
                &variables,
                Arc::new(InstantQueryClock::new(0, 0)),
            );
            for (operator, result) in [
                ("<", expected == Some(Ordering::Less)),
                (
                    "<=",
                    matches!(expected, Some(Ordering::Less | Ordering::Equal)),
                ),
                (">", expected == Some(Ordering::Greater)),
                (
                    ">=",
                    matches!(expected, Some(Ordering::Greater | Ordering::Equal)),
                ),
                ("=", expected == Some(Ordering::Equal)),
                ("<>", expected != Some(Ordering::Equal)),
            ] {
                let text = format!("left {operator} right");
                let expression = drasi_query_cypher::parse_expression(&text).expect("comparison");
                assert_eq!(
                    evaluator
                        .evaluate_expression(&context, &expression)
                        .await
                        .expect("numeric comparison"),
                    VariableValue::Bool(result),
                    "{left:?} / {right:?}: {text}"
                );
            }
        }
    }
}
