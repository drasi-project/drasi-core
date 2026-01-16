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

use crate::evaluation::functions::{Cos, Degrees, Function, Pi, Radians, Sin, Tan};
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock};

use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

fn create_trig_expression_test_function_registry() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());

    registry.register_function("cos", Function::Scalar(Arc::new(Cos {})));
    registry.register_function("degrees", Function::Scalar(Arc::new(Degrees {})));
    registry.register_function("pi", Function::Scalar(Arc::new(Pi {})));
    registry.register_function("radians", Function::Scalar(Arc::new(Radians {})));
    registry.register_function("sin", Function::Scalar(Arc::new(Sin {})));
    registry.register_function("tan", Function::Scalar(Arc::new(Tan {})));

    registry
}

#[tokio::test]
async fn evaluate_pi() {
    let expr = "pi()";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64(std::f64::consts::PI).unwrap())
    );
}

#[tokio::test]
async fn evaluate_sin() {
    let expr = "sin(0.5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64((0.5_f64).sin()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_cos() {
    let expr = "cos(0.5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64((0.5_f64).cos()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_tan() {
    let expr = "tan(0.5)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64((0.5_f64).tan()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_degrees() {
    let expr = "degrees(pi())";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64(std::f64::consts::PI.to_degrees()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_radians() {
    let expr = "radians(180)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64(std::f64::consts::PI).unwrap())
    );
}

#[tokio::test]
async fn evaluate_sin_pi() {
    let expr = "sin(pi())";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64(std::f64::consts::PI.sin()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_cos_pi() {
    let expr = "cos(pi())";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64(std::f64::consts::PI.cos()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_tan_pi() {
    let expr = "tan(pi())";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64(std::f64::consts::PI.tan()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_sin_pi_over_2() {
    let expr = "sin(pi()/2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64((std::f64::consts::PI / 2.0).sin()).unwrap())
    );
}

#[tokio::test]
async fn evaluate_cos_pi_over_2() {
    let expr = "cos(pi()/2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = create_trig_expression_test_function_registry();
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
        VariableValue::Float(Float::from_f64((std::f64::consts::PI / 2.0).cos()).unwrap())
    );
}
