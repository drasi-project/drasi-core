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

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::functions::{Function, Reduce};
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use std::collections::BTreeMap;
use std::sync::Arc;

fn create_reduce_expression_test_function_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());

    registry.register_function("reduce", Function::LazyScalar(Arc::new(Reduce::new())));

    registry
}

#[tokio::test]
async fn evaluate_reduce_func() {
    let expr = "reduce(acc = 0, x IN [1,2,3] | acc + x)";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_reduce_expression_test_function_registry();
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

    let function_registry = create_reduce_expression_test_function_registry();
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

    let function_registry = create_reduce_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let sensor_val_version_list = vec![
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(11)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(86)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(3)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(121)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
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

    let function_registry = create_reduce_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let sensor_val_version_list = vec![
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(11)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(86)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(3)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(121)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
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

#[tokio::test]
async fn evaluate_reduce_func_with_filter() {
    let expr = "reduce (count = 0, val IN $param1 where val.value > 0| count + 1)";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_reduce_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let sensor_val_version_list = vec![
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(11)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(86)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(3)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(121)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
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
        VariableValue::Integer(4.into())
    );
}

#[tokio::test]
async fn evaluate_reduce_func_with_filter_no_results() {
    let expr = "reduce (count = 0, val IN $param1 where 1 = 0| count + 1)";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_reduce_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    let sensor_val_version_list = vec![
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(11)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(86)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(3)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
                VariableValue::Integer(Integer::from(121)),
            );
            map
        }),
        VariableValue::Object({
            let mut map = BTreeMap::new();
            map.insert(
                "value".to_string(),
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
        VariableValue::Integer(0.into())
    );
}
