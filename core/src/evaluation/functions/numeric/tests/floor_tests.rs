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

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::numeric::Floor;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

#[tokio::test]
async fn floor_too_few_args() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Real(0.0)),
        )],
        position_in_query: 10,
    };

    let result = floor.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn floor_too_many_args() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(Integer::from(0)),
        VariableValue::Integer(Integer::from(0)),
    ];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Real(0.0)),
        )],
        position_in_query: 10,
    };

    let result = floor.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn floor_null() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        )],
        position_in_query: 10,
    };

    let result = floor
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn floor_zero_float() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(0_f64).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        )],
        position_in_query: 10,
    };

    let result = floor
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn floor_negative_int() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-1_f64).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        )],
        position_in_query: 10,
    };

    let result = floor
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1_f64);
}

#[tokio::test]
async fn floor_negative_float() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-0.001).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        )],
        position_in_query: 10,
    };

    let result = floor
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1_f64);
}

#[tokio::test]
async fn floor_positive_int() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((1_f64).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        )],
        position_in_query: 10,
    };

    let result = floor
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();

    assert_eq!(result, 1_f64);
}

#[tokio::test]
async fn floor_positive_float() {
    let floor = Floor {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1.001).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("floor"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        )],
        position_in_query: 10,
    };

    let result = floor
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1_f64);
}
