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

use crate::evaluation::functions::{Abs, Ceil, Floor, Function, Round, Sign};
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

fn create_numeric_expression_test_function_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());

    registry.register_function("abs", Function::Scalar(Arc::new(Abs {})));
    registry.register_function("ceil", Function::Scalar(Arc::new(Ceil {})));
    registry.register_function("floor", Function::Scalar(Arc::new(Floor {})));
    registry.register_function("round", Function::Scalar(Arc::new(Round {})));
    registry.register_function("sign", Function::Scalar(Arc::new(Sign {})));

    registry
}

#[tokio::test]
async fn evaluate_abs() {
    let expr = "abs(-5.5) + abs(3.6) - abs($param1) + abs($param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_numeric_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(-3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(5)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Float(Float::from_f64(11.1).unwrap())
        );
    }
}

#[tokio::test]
async fn evaluate_ceil() {
    let expr = "ceil(-5.5) + ceil(3.6) - ceil($param1) + ceil($param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_numeric_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(-3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(5)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Float(Float::from_f64(7_f64).unwrap())
        );
    }
}

#[tokio::test]
async fn evaluate_floor() {
    let expr = "floor(-5.5) + floor(3.6) - floor($param1) + floor($param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_numeric_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(-3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(5)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Float(Float::from_f64(5_f64).unwrap())
        );
    }
}

#[tokio::test]
async fn evaluate_round() {
    let expr = "round(-5.5) + round(3.6) - round($param1) + round($param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_numeric_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(-3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(5)));
    {
        let context =
            ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
        assert_eq!(
            evaluator
                .evaluate_expression(&context, &expr)
                .await
                .unwrap(),
            VariableValue::Float(Float::from_f64(7_f64).unwrap())
        );
    }
}

#[tokio::test]
async fn evaluate_sign() {
    let expr = "sign(-5.5) + sign(0) - sign($param1) + sign($param2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_numeric_expression_test_function_registry();
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let mut variables = QueryVariables::new();

    variables.insert("param1".into(), VariableValue::Integer(Integer::from(-3)));
    variables.insert("param2".into(), VariableValue::Integer(Integer::from(5)));
    {
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
}
