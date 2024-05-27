use std::sync::Arc;

use drasi_query_ast::ast;

use super::text;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_to_lower() {
    let to_lower = text::ToLower {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("HELLO".to_string())];
    let result = to_lower
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::String("hello".to_string()));
}

#[tokio::test]
async fn test_to_lower_too_many_args() {
    let to_lower = text::ToLower {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("HELLO".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = to_lower
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_to_lower_too_few_args() {
    let to_lower = text::ToLower {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = to_lower
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_to_lower_invalid_args() {
    let to_lower = text::ToLower {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = to_lower
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}
