use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use std::collections::BTreeMap;
use std::sync::Arc;

#[tokio::test]
async fn evaluate_reduce_func() {
    let expr = "reduce(acc = 0, x IN [1,2,3] | acc + x)";

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
        VariableValue::Integer(Integer::from(6))
    );
}

#[tokio::test]
async fn evaluate_reduce_func_min_temp() {
    let expr = "reduce ( minTemp = 0.0, sensorValVersion IN [11.2, 431, -75, 24] | 
        CASE WHEN sensorValVersion < minTemp THEN sensorValVersion
          ELSE minTemp END)";

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
        VariableValue::Integer(Integer::from(-75))
    );
}

#[tokio::test]
async fn evaluate_reduce_func_sensor_value() {
    let expr = "reduce (total = 0.0, val IN $param1| total + val.value)";

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
        VariableValue::Float(176.0.into())
    );
}

#[tokio::test]
async fn evaluate_reduce_func_sensor_value_count() {
    let expr = "reduce (count = 0, val IN $param1| count + 1)";

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
        VariableValue::Integer(5.into())
    );
}
