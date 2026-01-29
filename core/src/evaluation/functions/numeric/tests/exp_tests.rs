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
use crate::evaluation::functions::numeric::{E, Exp};
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

// ============================================================================
// Tests for E (Euler's number constant)
// ============================================================================

#[tokio::test]
async fn e_returns_eulers_number() {
    let e = E {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("e"),
        args: vec![],
        position_in_query: 10,
    };

    let result = e.call(&context, &func_expr, args).await.unwrap();
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            // Euler's number is approximately 2.71828182845904523536...
            assert!((value - std::f64::consts::E).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn e_with_args_fails() {
    let e = E {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(0.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("e"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = e.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn e_multiple_args_fails() {
    let e = E {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
    ];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("e"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = e.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

// ============================================================================
// Tests for Exp (exponential function: e^x)
// ============================================================================

#[tokio::test]
async fn exp_too_few_args() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn exp_too_many_args() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
    ];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn exp_null() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn exp_zero_int() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(0.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(0) = 1
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            assert!((value - 1.0).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_one_int() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(1)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(1) = e
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            assert!((value - std::f64::consts::E).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_negative_int() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer((-1).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(-1)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(-1) = 1/e ≈ 0.36787944117...
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            let expected = (-1_f64).exp();
            assert!((value - expected).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_positive_int() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(2.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(2)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(2) = e^2 ≈ 7.3890560989...
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            let expected = (2_f64).exp();
            assert!((value - expected).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_zero_float() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((0.0).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(0.0) = 1
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            assert!((value - 1.0).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_one_float() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((1.0).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(1)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(1.0) = e
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            assert!((value - std::f64::consts::E).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_negative_float() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((-0.5).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(-0.5) ≈ 0.60653665862...
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            let expected = (-0.5_f64).exp();
            assert!((value - expected).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_positive_float() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((2.5).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await.unwrap();
    // exp(2.5) ≈ 12.1824939607...
    match result {
        VariableValue::Float(f) => {
            let value = f.as_f64().unwrap();
            let expected = (2.5_f64).exp();
            assert!((value - expected).abs() < 1e-10);
        }
        _ => panic!("Expected Float, got {:?}", result),
    }
}

#[tokio::test]
async fn exp_invalid_argument_type() {
    let exp = Exp {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("invalid".into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("exp"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = exp.call(&context, &func_expr, args).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}
