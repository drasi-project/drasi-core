use std::sync::Arc;

use drasi_query_ast::ast;

use super::text;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_to_string() {
    let to_string = text::ToString {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::String("123".to_string()));

    let to_string = text::ToString {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
        VariableValue::Integer(3.into()),
    ])];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::String("[1, 2, 3]".to_string())
    );
}

#[tokio::test]
async fn test_to_string_too_many_args() {
    let to_string = text::ToString {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(123.into()),
        VariableValue::Integer(123.into()),
    ];
    let result = to_string
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
async fn test_to_string_too_few_args() {
    let to_string = text::ToString {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = to_string
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
async fn test_to_string_null() {
    let to_string = text::ToString {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}
