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
async fn test_reverse() {
    let reverse = text::Reverse {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
    ];
    let result = reverse.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("isard".to_string()));

    let args = vec![
        VariableValue::String("tenet".to_string()),
    ];
    let result = reverse.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("tenet".to_string()));
}

#[tokio::test]
async fn test_reverse_invalid_inputs() {
    let reverse = text::Reverse {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(10.into()),
    ];
    let result = reverse.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}

#[tokio::test]
async fn test_reverse_too_many_args() {
    let reverse = text::Reverse {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::String("drasi".to_string()),
    ];
    let result = reverse.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidArgumentCount(_)));
}

#[tokio::test]
async fn test_reverse_null() {
    let reverse = text::Reverse {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Null,
    ];
    let result = reverse.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}