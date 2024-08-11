use std::sync::Arc;

use drasi_query_ast::ast;

// use serde_json::{Value, Number};
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::numeric::Ceil;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError,  FunctionEvaluationError, ExpressionEvaluationContext, InstantQueryClock};

#[tokio::test]
async fn ceil_too_few_args() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn ceil_too_many_args() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(0.into()),
        VariableValue::Integer(0.into()),
    ];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn ceil_null() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn ceil_zero_float() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float((0_f64).into())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn ceil_negative_int() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-1_f64).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, -1_f64);
}

#[tokio::test]
async fn ceil_negative_float() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-1.001).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, -1_f64);
}

#[tokio::test]
async fn ceil_positive_int() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1_f64).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, 1_f64);
}

#[tokio::test]
async fn ceil_positive_float() {
    let ceil = Ceil {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1.001).unwrap())];
    let func_expr = ast::FunctionExpression {
        name: Arc::from("ceil"),
        args: vec![ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Integer(0)),
        )],
        position_in_query: 10,
    };

    let result = ceil.call(&context, &func_expr, args.clone()).await.unwrap();
    assert_eq!(result, 2_f64);
}
