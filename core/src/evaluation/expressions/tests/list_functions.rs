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
use crate::evaluation::functions::{Distinct, Function, IndexOf, Insert, Range, Reverse, Tail};
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator, FunctionError,
    FunctionEvaluationError, InstantQueryClock,
};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

use std::sync::Arc;

fn create_list_expression_test_function_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());

    registry.register_function("tail", Function::Scalar(Arc::new(Tail {})));
    registry.register_function("reverse", Function::Scalar(Arc::new(Reverse {})));
    registry.register_function("range", Function::Scalar(Arc::new(Range {})));
    registry.register_function("coll.distinct", Function::Scalar(Arc::new(Distinct {})));
    registry.register_function("coll.indexOf", Function::Scalar(Arc::new(IndexOf {})));
    registry.register_function("coll.insert", Function::Scalar(Arc::new(Insert {})));

    registry
}

#[tokio::test]
async fn test_list_tail() {
    let expr = "tail(['one','two','three'])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::String("two".to_string()),
            VariableValue::String("three".to_string())
        ])
    );
}

#[tokio::test]
async fn test_list_tail_multiple_arguments() {
    let expr = "tail(['one','two','three'], 3, ['four','five','six'])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _name,
            error: FunctionEvaluationError::InvalidArgumentCount
        })
    ));
}

#[tokio::test]
async fn test_list_tail_null() {
    let expr = "tail(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
async fn test_list_reverse() {
    let expr = "reverse(['one','two',null, 'null', 123, 4.23])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::Float(Float::from(4.23)),
            VariableValue::Integer(Integer::from(123)),
            VariableValue::String("null".to_string()),
            VariableValue::Null,
            VariableValue::String("two".to_string()),
            VariableValue::String("one".to_string())
        ])
    );
}

#[tokio::test]
async fn test_list_reverse_null() {
    let expr = "reverse(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
async fn test_list_reverse_multiple_arguments() {
    let expr = "reverse(['one','two','three'], 3, ['four','five','six'])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        })
    ));
}

#[tokio::test]
async fn test_list_range() {
    let expr = "range(0,10)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::Integer(Integer::from(0)),
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(3)),
            VariableValue::Integer(Integer::from(4)),
            VariableValue::Integer(Integer::from(5)),
            VariableValue::Integer(Integer::from(6)),
            VariableValue::Integer(Integer::from(7)),
            VariableValue::Integer(Integer::from(8)),
            VariableValue::Integer(Integer::from(9)),
            VariableValue::Integer(Integer::from(10))
        ])
    );
}

#[tokio::test]
async fn test_list_range_step() {
    let expr = "range(2,18,3)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(5)),
            VariableValue::Integer(Integer::from(8)),
            VariableValue::Integer(Integer::from(11)),
            VariableValue::Integer(Integer::from(14)),
            VariableValue::Integer(Integer::from(17))
        ])
    );
}

#[tokio::test]
async fn test_list_range_invalid_arg_count() {
    let expr = "range(2,18,3,4)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        })
    ));

    let expr = "range(2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        })
    ));
}

#[tokio::test]
async fn test_list_range_invalid_arguments() {
    let expr = "range(2,'18')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(1)
        })
    ));
}

#[tokio::test]
async fn test_list_range_negative_numbers() {
    let expr = "range(-5, 5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::Integer(Integer::from(-5)),
            VariableValue::Integer(Integer::from(-4)),
            VariableValue::Integer(Integer::from(-3)),
            VariableValue::Integer(Integer::from(-2)),
            VariableValue::Integer(Integer::from(-1)),
            VariableValue::Integer(Integer::from(0)),
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(3)),
            VariableValue::Integer(Integer::from(4)),
            VariableValue::Integer(Integer::from(5))
        ])
    );
}

#[tokio::test]
async fn test_list_range_all_negative() {
    let expr = "range(-10, -5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::Integer(Integer::from(-10)),
            VariableValue::Integer(Integer::from(-9)),
            VariableValue::Integer(Integer::from(-8)),
            VariableValue::Integer(Integer::from(-7)),
            VariableValue::Integer(Integer::from(-6)),
            VariableValue::Integer(Integer::from(-5))
        ])
    );
}

#[tokio::test]
async fn test_list_range_start_greater_than_end() {
    let expr = "range(10, 5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    // When start > end, the range is empty
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![])
    );
}

#[tokio::test]
async fn test_list_range_single_element() {
    let expr = "range(5, 5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
        VariableValue::List(vec![VariableValue::Integer(Integer::from(5))])
    );
}

#[tokio::test]
async fn test_list_range_negative_with_step() {
    let expr = "range(-10, 10, 5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
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
            VariableValue::Integer(Integer::from(-10)),
            VariableValue::Integer(Integer::from(-5)),
            VariableValue::Integer(Integer::from(0)),
            VariableValue::Integer(Integer::from(5)),
            VariableValue::Integer(Integer::from(10))
        ])
    );
}

#[tokio::test]
async fn test_list_range_large_step() {
    let expr = "range(0, 10, 100)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    // Step is larger than range, should only include start
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![VariableValue::Integer(Integer::from(0))])
    );
}

#[tokio::test]
async fn test_list_range_invalid_step_argument() {
    let expr = "range(0, 10, '2')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(2)
        })
    ));
}

#[tokio::test]
async fn test_list_range_float_arguments() {
    let expr = "range(0.5, 10.5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        })
    ));
}

#[tokio::test]
#[should_panic(expected = "assertion failed: step != 0")]
async fn test_list_range_zero_step() {
    // Note: This test documents that step_by(0) causes a panic
    // This is a limitation of the current implementation using Rust's step_by
    let expr = "range(0, 10, 0)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    // This will panic with "assertion failed: step != 0" error
    let _ = evaluator.evaluate_expression(&context, &expr).await;
}

#[tokio::test]
async fn test_list_range_negative_step() {
    let expr = "range(0, 10, -1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    // Negative step is cast to usize, resulting in a very large step
    // This should result in only the first element
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(
        result,
        VariableValue::List(vec![VariableValue::Integer(Integer::from(0))])
    );
}

#[tokio::test]
async fn test_distinct() {
    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let expr = "coll.distinct([1, 2, 2, 3, 1])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(
        result,
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(3)),
        ])
    );
}

#[tokio::test]
async fn test_index_of() {
    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let expr = "coll.indexOf(['a', 'b', 'c'], 'b')";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Integer(Integer::from(1)));
}

#[tokio::test]
async fn test_insert() {
    let function_registry = create_list_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());
    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    let expr = "coll.insert([1, 2, 3], 1, 99)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap();
    assert_eq!(
        result,
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(99)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(3)),
        ])
    );
}
