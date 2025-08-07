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

use drasi_core::evaluation::context::QueryVariables;
use drasi_core::evaluation::functions::ScalarFunction;
use drasi_core::evaluation::variable_value::VariableValue;
use drasi_core::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

use crate::gql_scalar::Cast;

fn get_func_expr() -> drasi_core::evaluation::functions::ast::FunctionExpression {
    drasi_core::evaluation::functions::ast::FunctionExpression {
        name: Arc::from("cast"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_cast_to_string() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(123.into()),
        VariableValue::String("STRING".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::String("123".to_string()));
}

#[tokio::test]
async fn test_cast_to_integer() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("456".to_string()),
        VariableValue::String("INTEGER".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Integer(456.into()));
}

#[tokio::test]
async fn test_cast_to_int() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("789".to_string()),
        VariableValue::String("INT".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Integer(789.into()));
}

#[tokio::test]
async fn test_cast_to_float() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("2.5".to_string()),
        VariableValue::String("FLOAT".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Float(2.5.into()));
}

#[tokio::test]
async fn test_cast_to_boolean() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("true".to_string()),
        VariableValue::String("BOOLEAN".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));
}

#[tokio::test]
async fn test_cast_to_bool() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("false".to_string()),
        VariableValue::String("BOOL".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_cast_case_insensitive() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(42.into()),
        VariableValue::String("string".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::String("42".to_string()));
}

#[tokio::test]
async fn test_cast_invalid_type() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(123.into()),
        VariableValue::String("INVALID".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidType { .. }
        }
    ));
}

#[tokio::test]
async fn test_cast_too_many_args() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(123.into()),
        VariableValue::String("STRING".to_string()),
        VariableValue::String("EXTRA".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_cast_too_few_args() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_cast_invalid_target_type_arg() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(123.into()),
        VariableValue::Integer(456.into()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(1)
        }
    ));
}

#[tokio::test]
async fn test_cast_null_value() {
    let cast = Cast {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Null,
        VariableValue::String("STRING".to_string()),
    ];
    let result = cast.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}
