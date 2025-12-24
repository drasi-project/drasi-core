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
use crate::evaluation::functions::cypher_scalar::null_if::NullIf;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("nullIf"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_null_if_equivalent_integers() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(4.into()),
        VariableValue::Integer(4.into()),
    ];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_null_if_non_equivalent_strings() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("abc".to_string()),
        VariableValue::String("def".to_string()),
    ];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::String("abc".to_string()));
}

#[tokio::test]
async fn test_null_if_both_null() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null, VariableValue::Null];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_null_if_first_null_second_not() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null, VariableValue::Integer(5.into())];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_null_if_wrong_arg_count_zero() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_null_if_wrong_arg_count_one() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(4.into())];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_null_if_wrong_arg_count_three() {
    let null_if = NullIf {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(4.into()),
        VariableValue::Integer(4.into()),
        VariableValue::Integer(4.into()),
    ];
    let result = null_if.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}
