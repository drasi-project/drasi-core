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

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::numeric::E;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};
use crate::evaluation::{FunctionError, FunctionEvaluationError};
use drasi_query_ast::ast;

#[tokio::test]
async fn test_e() {
    let func = E {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let expression = ast::FunctionExpression {
        name: Arc::from("e"),
        args: vec![],
        position_in_query: 0,
    };

    let result = func.call(&context, &expression, vec![]).await.unwrap();
    assert!(result.is_f64());
    assert_eq!(result.as_f64().unwrap(), std::f64::consts::E);
}

#[tokio::test]
async fn test_e_arg_count() {
    let func = E {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let expression = ast::FunctionExpression {
        name: Arc::from("e"),
        args: vec![],
        position_in_query: 0,
    };

    // Test with 1 argument (should be 0)
    let result = func
        .call(
            &context,
            &expression,
            vec![VariableValue::Integer(1.into())],
        )
        .await;

    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}
