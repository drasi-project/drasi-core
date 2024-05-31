use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::cypher_scalar::{ToBoolean, ToBooleanOrNull};
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_to_boolean_integer() {
    let to_boolean = ToBoolean {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(1.into())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));

    let args = vec![VariableValue::Integer(0.into())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_to_boolean_string() {
    let to_boolean = ToBoolean {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("true".to_string())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));

    let args = vec![VariableValue::String("false".to_string())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));

    let args = vec![VariableValue::String("foo".to_string())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_to_boolean_boolean() {
    let to_boolean = ToBoolean {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Bool(true)];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Bool(true));

    let args = vec![VariableValue::Bool(false)];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Bool(false));
}

#[tokio::test]
async fn test_to_boolean_invalid_inputs() {
    // Should return an error

    let to_boolean = ToBoolean {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(1.0.into())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_to_boolean_or_null_invalid_inputs() {
    // Should just return a null
    let to_boolean = ToBooleanOrNull {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(1.0.into())];
    let result = to_boolean
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}
