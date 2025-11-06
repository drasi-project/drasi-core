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

use std::sync::Arc;

use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

#[tokio::test]
async fn evaluate_logical_predicate() {
    let expr = "$param1 = 1 AND ($param2 = 2 OR $param3 = 3)";
    let predicate = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();
    variables.insert("param1".into(), VariableValue::Integer(Integer::from(1)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param3".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert!(evaluator
            .evaluate_predicate(&context, &predicate)
            .await
            .unwrap());
    }

    variables.insert("param3".into(), VariableValue::Integer(Integer::from(4)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert!(evaluator
            .evaluate_predicate(&context, &predicate)
            .await
            .unwrap());
    }

    variables.insert("param2".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert!(!evaluator
            .evaluate_predicate(&context, &predicate)
            .await
            .unwrap());
    }

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(2)));
    variables.insert("param3".into(), VariableValue::Integer(Integer::from(3)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert!(!evaluator
            .evaluate_predicate(&context, &predicate)
            .await
            .unwrap());
    }
}

#[tokio::test]
async fn evaluate_not() {
    let expr = "NOT ($param1 = $param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::String(String::from("a")));
    variables.insert("param2".into(), VariableValue::String(String::from("a")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }

    variables.insert("param1".into(), VariableValue::String(String::from("b")));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }
}

#[tokio::test]
async fn evaluate_and() {
    let expr = "$param1 AND $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Bool(true));
    variables.insert("param2".into(), VariableValue::Bool(true));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }

    variables.insert("param1".into(), VariableValue::Bool(false));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }
}

#[tokio::test]
async fn evaluate_or() {
    let expr = "$param1 OR $param2";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Bool(true));
    variables.insert("param2".into(), VariableValue::Bool(false));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(true)
        );
    }

    variables.insert("param1".into(), VariableValue::Bool(false));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Bool(false)
        );
    }
}

#[tokio::test]
async fn and_not_short_circuited() {
    // Tests that AND evaluates both operands even when the first is false.
    // In a short-circuit evaluation (like Rust's && operator), when the left operand
    // is false, the right operand would not be evaluated and the result would be false.
    //
    // This test verifies non-short-circuit behavior by having the right operand
    // cause a DivideByZero error. If AND were short-circuited, the error would never
    // occur and we'd just get Bool(false).
    let expr = "$param1 AND (1 / 0 = 1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    // Set first operand to false - in a short-circuit evaluation,
    // the second operand would not be evaluated
    variables.insert("param1".into(), VariableValue::Bool(false));

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    // Evaluate the AND expression
    let result = evaluator.evaluate_expression(&context, &expr).await;

    // Assert that we get an error (DivideByZero), NOT Bool(false)
    // An error proves the second operand was evaluated (non-short-circuit behavior)
    // Bool(false) would indicate short-circuit evaluation
    assert!(
        result.is_err(),
        "Expected DivideByZero error (proving both operands evaluated), but got: {:?}",
        result
    );
}

#[tokio::test]
async fn or_not_short_circuited() {
    // Tests that OR evaluates both operands even when the first is true.
    // In a short-circuit evaluation (like Rust's || operator), when the left operand
    // is true, the right operand would not be evaluated and the result would be true.
    //
    // This test verifies non-short-circuit behavior by having the right operand
    // cause a DivideByZero error. If OR were short-circuited, the error would never
    // occur and we'd just get Bool(true).
    let expr = "$param1 OR (1 / 0 = 1)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    // Set first operand to true - in a short-circuit evaluation,
    // the second operand would not be evaluated
    variables.insert("param1".into(), VariableValue::Bool(true));

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));

    // Evaluate the OR expression
    let result = evaluator.evaluate_expression(&context, &expr).await;

    // Assert that we get an error (DivideByZero), NOT Bool(true)
    // An error proves the second operand was evaluated (non-short-circuit behavior)
    // Bool(true) would indicate short-circuit evaluation
    assert!(
        result.is_err(),
        "Expected DivideByZero error (proving both operands evaluated), but got: {:?}",
        result
    );
}
