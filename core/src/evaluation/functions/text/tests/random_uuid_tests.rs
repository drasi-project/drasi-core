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
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("randomUUID"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_random_uuid() {
    let random_uuid = text::RandomUUID {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = random_uuid
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();

    // Should return a string
    match result {
        VariableValue::String(s) => {
            // UUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx (36 chars)
            assert_eq!(s.len(), 36);
            assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
        }
        _ => panic!("Expected String"),
    }
}

#[tokio::test]
async fn test_random_uuid_too_many_args() {
    let random_uuid = text::RandomUUID {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("arg".to_string())];
    let result = random_uuid
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_random_uuid_unique() {
    let random_uuid = text::RandomUUID {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result1 = random_uuid
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    let result2 = random_uuid
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();

    // Two calls should return different UUIDs
    assert_ne!(result1, result2);
}
