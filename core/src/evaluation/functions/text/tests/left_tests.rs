use std::sync::Arc;

use drasi_query_ast::ast;

use super::text;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::{ScalarFunction};
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError,FunctionEvaluationError, ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_left() {
    let left = text::Left {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(3.into()),
    ];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("dra".to_string()));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(10.into()),
    ];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("drasi".to_string()));
}

#[tokio::test]
async fn test_left_invalid_inputs() {
    let left = text::Left {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::String("drasi".to_string()),
    ];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), FunctionError { function_name: _, error: FunctionEvaluationError::InvalidArgument(1) }));
}

#[tokio::test]
async fn test_left_invalid_input_value() {
    let left = text::Left {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer((-1).into()),
    ];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    let error = result.unwrap_err();
    assert!(matches!(error,  FunctionError { function_name: _, error: FunctionEvaluationError::OverflowError }));
}

#[tokio::test]
async fn test_left_too_many_args() {
    let left = text::Left {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(3.into()),
        VariableValue::String("hello".to_string()),
    ];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_left_too_few_args() {
    let left = text::Left {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("drasi".to_string())];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));

    let args = vec![];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount
        }
    ));
}

#[tokio::test]
async fn test_left_null() {
    let left = text::Left {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null, VariableValue::Integer(3.into())];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![VariableValue::Null, VariableValue::Null];
    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Null,
    ];

    let result = left.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), FunctionError {
        function_name: _,
        error: FunctionEvaluationError::InvalidArgument(1)
    }));
}
