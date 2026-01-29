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
use crate::evaluation::functions::numeric::{Ln, Log10};
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    ExpressionEvaluationContext, FunctionError, FunctionEvaluationError, InstantQueryClock,
};

// ============================================================================
// Tests for Ln function
// ============================================================================

#[tokio::test]
async fn ln_too_few_args() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn ln_too_many_args() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
    ];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn ln_null() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn ln_positive_int() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(10.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await.unwrap();
    // ln(10) ≈ 2.302585
    match result {
        VariableValue::Float(f) => {
            let val = f.as_f64().unwrap();
            assert!((val - 2.302585).abs() < 0.0001);
        }
        _ => panic!("Expected Float result"),
    }
}

#[tokio::test]
async fn ln_positive_float() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((2.71828).into())]; // approximately e
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await.unwrap();
    // ln(e) ≈ 1
    match result {
        VariableValue::Float(f) => {
            let val = f.as_f64().unwrap();
            assert!((val - 1.0).abs() < 0.001);
        }
        _ => panic!("Expected Float result"),
    }
}

#[tokio::test]
async fn ln_zero_int() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(0.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn ln_zero_float() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((0.0).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn ln_negative_int() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer((-5).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn ln_negative_float() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((-2.5).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn ln_invalid_type() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("not a number".into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

// ============================================================================
// Tests for Log10 function
// ============================================================================

#[tokio::test]
async fn log10_too_few_args() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn log10_too_many_args() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
    ];

    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn log10_null() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn log10_positive_int() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(100.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    // log10(100) = 2.0
    match result {
        VariableValue::Float(f) => {
            let val = f.as_f64().unwrap();
            assert!((val - 2.0).abs() < 0.0001);
        }
        _ => panic!("Expected Float result"),
    }
}

#[tokio::test]
async fn log10_positive_float() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((1000.0).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    // log10(1000) = 3.0
    match result {
        VariableValue::Float(f) => {
            let val = f.as_f64().unwrap();
            assert!((val - 3.0).abs() < 0.0001);
        }
        _ => panic!("Expected Float result"),
    }
}

#[tokio::test]
async fn log10_one() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10
        .call(&context, &func_expr, args.clone())
        .await
        .unwrap();
    // log10(1) = 0.0
    match result {
        VariableValue::Float(f) => {
            let val = f.as_f64().unwrap();
            assert!((val - 0.0).abs() < 0.0001);
        }
        _ => panic!("Expected Float result"),
    }
}

#[tokio::test]
async fn log10_zero_int() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(0.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn log10_zero_float() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((0.0).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn log10_negative_int() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer((-10).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn log10_negative_float() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((-5.5).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}

#[tokio::test]
async fn log10_invalid_type() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("not a number".into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("log10"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = log10.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgument(0)
        }
    ));
}
