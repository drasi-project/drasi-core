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

use crate::evaluation::functions::numeric::{E, Exp};
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::{ExpressionEvaluationContext, FunctionEvaluationError, InstantQueryClock};
use drasi_query_ast::ast;

#[tokio::test]
async fn test_e() {
    let func = E {};
    let binding = QueryVariables::new();
    let context = ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let expression = ast::FunctionExpression {
        name: Arc::from("e"),
        args: vec![],
        position_in_query: 0,
    };

    let result = func.call(&context, &expression, vec![]).await.unwrap();
    assert_eq!(
        result,
        VariableValue::Float(Float::from_f64(std::f64::consts::E).unwrap())
    );
}

#[tokio::test]
async fn test_exp() {
    let func = Exp {};
    let binding = QueryVariables::new();
    let context = ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let expression = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![],
        position_in_query: 0,
    };

    // Test with integer 0
    let result = func
        .call(&context, &expression, vec![VariableValue::Integer(0.into())])
        .await
        .unwrap();
    assert_eq!(
        result,
        VariableValue::Float(Float::from_f64(1.0).unwrap())
    );

    // Test with integer 1
    let result = func
        .call(&context, &expression, vec![VariableValue::Integer(1.into())])
        .await
        .unwrap();
    assert_eq!(
        result,
        VariableValue::Float(Float::from_f64(std::f64::consts::E).unwrap())
    );

    // Test with float
    let result = func
        .call(&context, &expression, vec![VariableValue::Float(Float::from_f64(2.0).unwrap())])
        .await
        .unwrap();
    match result {
        VariableValue::Float(f) => {
            assert!((f.as_f64().unwrap() - std::f64::consts::E.powf(2.0)).abs() < 1e-10);
        }
        _ => panic!("Expected float result"),
    }

    // Test with negative integer (-1)
    let result = func
        .call(&context, &expression, vec![VariableValue::Integer((-1).into())])
        .await
        .unwrap();
    match result {
        VariableValue::Float(f) => {
            // exp(-1) = 1/e
            assert!((f.as_f64().unwrap() - (-1.0f64).exp()).abs() < 1e-10);
        }
        _ => panic!("Expected float result for negative input"),
    }
}

#[tokio::test]
async fn test_exp_null() {
    let func = Exp {};
    let binding = QueryVariables::new();
    let context = ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let expression = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![],
        position_in_query: 0,
    };

    let result = func
        .call(&context, &expression, vec![VariableValue::Null])
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn test_exp_overflow() {
    let func = Exp {};
    let binding = QueryVariables::new();
    let context = ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let expression = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![],
        position_in_query: 0,
    };

    // exp(1000) overflows f64
    let result = func
        .call(&context, &expression, vec![VariableValue::Integer(1000.into())])
        .await;

    assert!(result.is_err());
    let err = result.err().unwrap();
    assert!(matches!(err.error, FunctionEvaluationError::OverflowError));
}
