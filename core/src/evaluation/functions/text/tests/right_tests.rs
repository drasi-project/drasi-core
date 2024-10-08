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

use super::text;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_right() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::Integer(4.into()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("ello".to_string()));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::Integer(10.into()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("hello".to_string()));
}

#[tokio::test]
async fn test_right_too_many_args() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(3.into()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_right_too_few_args() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_right_invalid_input() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Float(Float::from_f64(3.0).unwrap()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    let err = result.unwrap_err();
    assert!(matches!(
        err,
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(1)
        }
    ));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    let err = result.unwrap_err();
    assert!(matches!(
        err,
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(1),
        }
    ));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(10.into()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("drasi".to_string()));
}

#[tokio::test]
async fn test_right_null() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null, VariableValue::Integer(3.into())];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![VariableValue::Null, VariableValue::Null];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Null,
    ];

    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(1)
        }
    ));
}
