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
use crate::evaluation::functions::numeric::Exp;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

#[tokio::test]
async fn exp_too_few_args() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn exp_too_many_args() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
    ];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(1)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn exp_null() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn exp_zero() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(0.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    assert_eq!(result, VariableValue::Float(1.0.into()));
}

#[tokio::test]
async fn exp_positive_int() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(1)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    assert_eq!(
        result,
        VariableValue::Float(std::f64::consts::E.into())
    );
}

#[tokio::test]
async fn exp_negative_int() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer((-1).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(-1)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    let VariableValue::Float(f) = result else { unreachable!() };
    assert_eq!(f.as_f64().unwrap(), (-1.0_f64).exp());
}

#[tokio::test]
async fn exp_positive_float() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(2.0.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(2)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    let VariableValue::Float(f) = result else { unreachable!() };
    assert_eq!(f.as_f64().unwrap(), (2.0_f64).exp());
}

#[tokio::test]
async fn exp_overflow() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1000.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(1000)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err().error,
        FunctionEvaluationError::OverflowError
    ));
}
