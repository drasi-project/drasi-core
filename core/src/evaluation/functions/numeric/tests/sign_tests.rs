use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::numeric::Sign;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("sign"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn sign_too_few_args() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];

    let result = sign.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn sign_too_many_args() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(Integer::from(0)),
        VariableValue::Integer(Integer::from(0)),
    ];

    let result = sign.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn sign_null() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn sign_zero_int() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(Integer::from(0))];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0);
}

#[tokio::test]
async fn sign_negative_int() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(Integer::from(-1))];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1);
}

#[tokio::test]
async fn sign_positive_int() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(Integer::from(1))];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1);
}

#[tokio::test]
async fn sign_zero_float() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(0_f64).unwrap())];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn sign_negative_float() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-1.001).unwrap())];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(-1_f64, result);
}

#[tokio::test]
async fn sign_positive_float() {
    let sign = Sign {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1.001).unwrap())];

    let result = sign
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1_f64);
}
