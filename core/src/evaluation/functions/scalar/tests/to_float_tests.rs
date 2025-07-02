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
use crate::evaluation::functions::scalar::{ToFloat, ToFloatOrNull};
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_to_float_integer() {
    let to_float = ToFloat {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1.into())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Float(1.0.into()));
}

#[tokio::test]
async fn test_to_float_float() {
    let to_float = ToFloat {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(11.5.into())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Float(11.5.into()));
}

#[tokio::test]
async fn test_to_float_string() {
    let to_float = ToFloat {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("11.5".to_string())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Float(11.5.into()));

    let args = vec![VariableValue::String("11".to_string())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Float(11.0.into()));
}

#[tokio::test]
async fn test_to_float_null() {
    let to_float = ToFloat {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_to_float_bool() {
    // Should raise an error

    let to_float = ToFloat {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Bool(true)];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_to_float_or_null_bool() {
    // Should return null
    let to_float = ToFloatOrNull {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Bool(true)];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_to_float_or_null_string() {
    let to_float = ToFloatOrNull {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("11.5".to_string())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Float(11.5.into()));

    let args = vec![VariableValue::String("-11".to_string())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Float((-11.0).into()));

    let args = vec![VariableValue::String("abc".to_string())];
    let result = to_float
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}
