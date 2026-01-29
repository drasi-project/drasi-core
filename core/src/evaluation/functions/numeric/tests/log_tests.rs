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
// Ln (natural logarithm) tests
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

    // ln(1) = 0
    let args = vec![VariableValue::Integer(1.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn ln_positive_float() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // ln(e) ≈ 1
    let args = vec![VariableValue::Float(std::f64::consts::E.into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ln"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ln.call(&context, &func_expr, args.clone()).await.unwrap();
    // Check that result is approximately 1.0
    if let VariableValue::Float(f) = result {
        let val = f.as_f64().unwrap();
        assert!((val - 1.0).abs() < 1e-10);
    } else {
        panic!("Expected Float result");
    }
}

#[tokio::test]
async fn ln_zero() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // ln(0) should return Null (domain error)
    let args = vec![VariableValue::Integer(0.into())];
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
async fn ln_zero_float() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // ln(0.0) should return Null (domain error)
    let args = vec![VariableValue::Float((0_f64).into())];
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
async fn ln_negative_int() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // ln(-1) should return Null (domain error)
    let args = vec![VariableValue::Integer((-1).into())];
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
async fn ln_negative_float() {
    let ln = Ln {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // ln(-1.5) should return Null (domain error)
    let args = vec![VariableValue::Float((-1.5).into())];
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

// ============================================================================
// Log10 (base-10 logarithm) tests
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

    // log10(10) = 1
    let args = vec![VariableValue::Integer(10.into())];
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
    assert_eq!(result, 1_f64);
}

#[tokio::test]
async fn log10_positive_float() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // log10(100.0) = 2
    let args = vec![VariableValue::Float((100_f64).into())];
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
    assert_eq!(result, 2_f64);
}

#[tokio::test]
async fn log10_one() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // log10(1) = 0
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
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn log10_zero() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // log10(0) should return Null (domain error)
    let args = vec![VariableValue::Integer(0.into())];
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
async fn log10_zero_float() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // log10(0.0) should return Null (domain error)
    let args = vec![VariableValue::Float((0_f64).into())];
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
async fn log10_negative_int() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // log10(-1) should return Null (domain error)
    let args = vec![VariableValue::Integer((-1).into())];
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
async fn log10_negative_float() {
    let log10 = Log10 {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    // log10(-1.5) should return Null (domain error)
    let args = vec![VariableValue::Float((-1.5).into())];
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
