use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn test_list_construction() {
    let expr = "[1, 23>34.0, FALSE, 'drasi', [1,2,3]]";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Bool(false),
            VariableValue::Bool(false),
            VariableValue::String(String::from("drasi")),
            VariableValue::List(vec![
                VariableValue::Integer(Integer::from(1)),
                VariableValue::Integer(Integer::from(2)),
                VariableValue::Integer(Integer::from(3)),
            ]),
        ])
    );
}

#[tokio::test]
async fn test_list_comprehension_property_where_only() {
    let expr = "[x IN [123,342,12,34,-12] where x > 0]"; //for each element in param1, return the value property

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(123)),
            VariableValue::Integer(Integer::from(342)),
            VariableValue::Integer(Integer::from(12)),
            VariableValue::Integer(Integer::from(34)),
        ])
    );
}

#[tokio::test]
async fn test_list_comprehension_property_with_mapping_only() {
    let expr = "[x IN [123,342,12,34,-12] | x - 10]"; //for each element in param1, return the value property

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(113)),
            VariableValue::Integer(Integer::from(332)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(24)),
            VariableValue::Integer(Integer::from(-22)),
        ])
    );
}

#[tokio::test]
async fn test_list_comprehension_property() {
    let expr = "[x IN [123,342,12,34,-12] where x > 0| x - 10]"; //for each element in param1, return the value property

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(113)),
            VariableValue::Integer(Integer::from(332)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(24)),
        ])
    );
}

#[tokio::test]
async fn test_list_comprehension_parameter() {
    let expr = "[x IN $param1 | x.value ]"; //for each element in param1, return the value property

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    let sensor_val_version_list = vec![
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string().into(),
                VariableValue::Integer(Integer::from(11)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string().into(),
                VariableValue::Integer(Integer::from(86)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string().into(),
                VariableValue::Integer(Integer::from(3)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string().into(),
                VariableValue::Integer(Integer::from(121)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string().into(),
                VariableValue::Integer(Integer::from(-45)),
            );
            map
        }),
    ];
    variables.insert(
        "param1".to_string().into(),
        VariableValue::List(sensor_val_version_list),
    );
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(11)),
            VariableValue::Integer(Integer::from(86)),
            VariableValue::Integer(Integer::from(3)),
            VariableValue::Integer(Integer::from(121)),
            VariableValue::Integer(Integer::from(-45)),
        ])
    );
}
