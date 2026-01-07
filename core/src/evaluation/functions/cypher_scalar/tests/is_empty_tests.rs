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

use std::collections::BTreeMap;
use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::cypher_scalar::is_empty::IsEmpty;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("isEmpty"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_is_empty_string_empty() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("".to_string())];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));
}

#[tokio::test]
async fn test_is_empty_string_not_empty() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("hello".to_string())];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_is_empty_list_empty() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![])];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));
}

#[tokio::test]
async fn test_is_empty_list_not_empty() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
    ])];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_is_empty_object_empty() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Object(BTreeMap::new())];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));
}

#[tokio::test]
async fn test_is_empty_object_not_empty() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let mut map = BTreeMap::new();
    map.insert(
        "key".to_string(),
        VariableValue::String("value".to_string()),
    );
    let args = vec![VariableValue::Object(map)];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_is_empty_null() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_is_empty_invalid_type() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(42.into())];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn test_is_empty_wrong_arg_count() {
    let is_empty = IsEmpty {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = is_empty.call(&context, &get_func_expr(), args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}
