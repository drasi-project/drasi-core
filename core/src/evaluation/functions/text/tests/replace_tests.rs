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
async fn test_replace() {
    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::String("l".to_string()),
        VariableValue::String("x".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("hexxo".to_string()));

    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("-reactive-graph is ...? reactive-graph can xxxx".to_string()),
        VariableValue::String("reactive-graph".to_string()),
        VariableValue::String("drasi".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(
        result.unwrap(),
        VariableValue::String("-drasi is ...? drasi can xxxx".to_string())
    );


    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::String("e".to_string()),
        VariableValue::String("t".to_string()),
    ];

    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("drasi".to_string()));
}

#[tokio::test]
async fn test_replace_empty_delimiter() {
    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::String("".to_string()),
        VariableValue::String("x".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("hello".to_string()));
}

#[tokio::test]
async fn test_replace_invalid_inputs() {
    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::String("l".to_string()),
        VariableValue::Integer(123.into()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}

#[tokio::test]
async fn test_replace_too_many_args() {
    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::String("l".to_string()),
        VariableValue::String("x".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_replace_too_few_args() {
    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::String("l".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}


#[tokio::test]
async fn test_replace_null() {
    let replace = text::Replace {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Null,
        VariableValue::String("l".to_string()),
        VariableValue::String("x".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::Null,
        VariableValue::String("x".to_string()),
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![
        VariableValue::String("hello".to_string()),
        VariableValue::String("l".to_string()),
        VariableValue::Null,
    ];
    let result = replace.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}