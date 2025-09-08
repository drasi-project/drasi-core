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
use crate::evaluation::functions::{
    CharLength, Coalesce, CypherLast, Function, Head, Size, Timestamp, ToBoolean, ToFloat,
    ToInteger,
};
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

use std::sync::Arc;

fn create_cypher_scalar_expression_test_function_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());

    registry.register_function("char_length", Function::Scalar(Arc::new(CharLength {})));
    registry.register_function(
        "character_length",
        Function::Scalar(Arc::new(CharLength {})),
    );
    registry.register_function("size", Function::Scalar(Arc::new(Size {})));
    registry.register_function("coalesce", Function::Scalar(Arc::new(Coalesce {})));
    registry.register_function("head", Function::Scalar(Arc::new(Head {})));
    registry.register_function("last", Function::Scalar(Arc::new(CypherLast {})));
    registry.register_function("timestamp", Function::Scalar(Arc::new(Timestamp {})));
    registry.register_function("toBoolean", Function::Scalar(Arc::new(ToBoolean {})));
    registry.register_function("toFloat", Function::Scalar(Arc::new(ToFloat {})));
    registry.register_function("toInteger", Function::Scalar(Arc::new(ToInteger {})));

    registry
}

#[tokio::test]
async fn evaluate_char_length_with_string() {
    let expr = "char_length('drasi')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Integer(Integer::from(5))
    );
}

#[tokio::test]
async fn evaluate_character_length_with_string() {
    let expr = "char_length('ReactiveGraph1')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Integer(Integer::from(14))
    );
}

#[tokio::test]
async fn evaluate_size_of_string() {
    let expr = "size('drasi')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Integer(Integer::from(5))
    );
}

#[tokio::test]
async fn evaluate_size_of_list() {
    let expr = "size([1,null,3,4,5])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Integer(Integer::from(5))
    );
}

#[tokio::test]
async fn evaluate_size_null() {
    let expr = "size(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Null
    );
}

#[tokio::test]
async fn test_coalesce() {
    let expr = "coalesce(null, 'a', null, 'b', null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::String("a".to_string())
    );
}

#[tokio::test]
async fn test_head() {
    let expr = "head([1,2,3,null,5])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Integer(Integer::from(1))
    );
}

#[tokio::test]
async fn test_head_null() {
    let expr = "head(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Null
    );
}

#[tokio::test]
async fn test_last() {
    let expr = "last([1,2,3,null,5])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
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
        VariableValue::Integer(Integer::from(5))
    );

    let expr = "last([1,2,3,null,5, null])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::Null
    );
}

#[tokio::test]
async fn evaluate_timestamp() {
    let expr = "timestamp()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = match evaluator.evaluate_expression(&context, &expr).await {
        Ok(result) => result,
        Err(e) => panic!("Error: {e:?}"),
    };

    let time_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let time_since_epoch = time_since_epoch.as_millis() as i64;

    //abs diff should be less than 500
    assert!((result.as_i64().unwrap() - time_since_epoch).abs() < 500);
}

#[tokio::test]
async fn evaluate_timestamp_too_many_args() {
    let expr = "timestamp(12)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator.evaluate_expression(&context, &expr).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn evalaute_to_boolean_string() {
    let expr = "toBoolean('true')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Bool(true));

    let expr = "toBoolean('false')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Bool(false));

    let expr = "toBoolean('foo')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn evaluate_to_boolean_bool() {
    let expr = "toBoolean(true)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Bool(true));

    let expr = "toBoolean(false)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Bool(false));
}

#[tokio::test]
async fn evaluate_to_boolean_null() {
    let expr = "toBoolean(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn evaluate_to_float_string() {
    let expr = "toFloat('12.34')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Float(12.34.into()));

    let expr = "toFloat('12')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Float(12.0.into()));

    let expr = "toFloat('foo')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn evaluate_to_float_numeric() {
    let expr = "toFloat(12)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Float(12.0.into()));

    let expr = "toFloat(12.34)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Float(12.34.into()));
}

#[tokio::test]
async fn evluate_to_float_null() {
    let expr = "toFloat(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn evaluate_to_integer_string() {
    let expr = "toInteger('12')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Integer(12.into()));

    let expr = "toInteger('12.34')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Integer(12.into()));

    let expr = "toInteger('foo')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn evaluate_to_integer_numeric() {
    let expr = "toInteger(12)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Integer(12.into()));

    let expr = "toInteger(12.34)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Integer(12.into()));
}

#[tokio::test]
async fn test_to_integer_null() {
    let expr = "toInteger(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_cypher_scalar_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}
