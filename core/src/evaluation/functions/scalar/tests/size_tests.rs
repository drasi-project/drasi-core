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
use crate::evaluation::functions::scalar::Size;
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
async fn test_size_string() {
    let size = Size {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("drasi".to_string())];
    let result = size.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Integer(5.into()));
}

#[tokio::test]
async fn test_size_null() {
    let size = Size {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let result = size.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_size_list() {
    let size = Size {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::String("one".to_string()),
        VariableValue::String("two".to_string()),
        VariableValue::String("three".to_string()),
    ])];
    let result = size.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Integer(3.into()));
}
