use std::sync::Arc;

use drasi_query_ast::ast;

use super::text;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
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
async fn test_right() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::Integer(2.into()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("lo".to_string()));
}

#[tokio::test]
async fn test_right_too_many_args() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(3.into()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_right_too_few_args() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_right_invalid_input() {
    let right = text::Right {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Float(Float::from_f64(3.0).unwrap()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(10.into()),
    ];
    let result = right.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}
