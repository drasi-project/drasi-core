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

use crate::evaluation::functions::drasi::DrasiMin;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::{context::QueryVariables, InstantQueryClock};
use drasi_query_ast::ast;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_drasi_min() {
    let min = DrasiMin {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Integer(1.into()),
    ])];

    let result = min
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0);
}

#[tokio::test]
async fn test_drasi_min_multiple_types() {
    let min = DrasiMin {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Null,
        VariableValue::Integer(1.into()),
        VariableValue::Float((std::f64::consts::PI).into()),
        VariableValue::String("test".into()),
        VariableValue::Bool(true),
    ])];

    let result = min
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Bool(true));
}

#[tokio::test]
async fn test_drasi_min_list_of_strings() {
    let min = DrasiMin {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::String("apple".into()),
        VariableValue::String("banana".into()),
    ])];

    let result = min
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, "apple");
}
