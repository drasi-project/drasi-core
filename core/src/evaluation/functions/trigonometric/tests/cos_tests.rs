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
use crate::evaluation::functions::trigonometric::Cos;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

#[tokio::test]
async fn cos_too_few_args() {
    let cos = Cos {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("cos"),
        args: vec![],
        position_in_query: 10,
    };

    let result = cos.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn cos_too_many_args() {
    let cos = Cos {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float((0.5).into()),
        VariableValue::Float((0.5).into()),
    ];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("cos"),
        args: vec![],
        position_in_query: 10,
    };

    let result = cos.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn cos_null() {
    let cos = Cos {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("cos"),
        args: vec![],
        position_in_query: 10,
    };

    let result = cos.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn cos_zero() {
    let cos = Cos {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((0_f64).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("cos"),
        args: vec![],
        position_in_query: 10,
    };

    let result = cos.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, 1_f64);
}

#[tokio::test]
async fn cos_float() {
    let cos = Cos {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((0.5).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("cos"),
        args: vec![],
        position_in_query: 10,
    };

    let result = cos.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, (0.5_f64).cos());
}

#[tokio::test]
async fn cos_integer() {
    let cos = Cos {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("cos"),
        args: vec![],
        position_in_query: 10,
    };

    let result = cos.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, (1_f64).cos());
}
