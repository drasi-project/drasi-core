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
async fn test_split_single_delimiter() {
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello,world".to_string()),
        VariableValue::String(",".to_string()),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(
        result.unwrap(),
        VariableValue::List(vec![
            VariableValue::String("hello".to_string()),
            VariableValue::String("world".to_string())
        ])
    );

    //A single delimiter with multiple characters
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String(
            "hello||world|| this is a test|| reactive-graph|| kubectl".to_string(),
        ),
        VariableValue::String("||".to_string()),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(
        result.unwrap(),
        VariableValue::List(vec![
            VariableValue::String("hello".to_string()),
            VariableValue::String("world".to_string()),
            VariableValue::String(" this is a test".to_string()),
            VariableValue::String(" reactive-graph".to_string()),
            VariableValue::String(" kubectl".to_string())
        ])
    );
}

#[tokio::test]
async fn test_split_invalid_inputs() {
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String(
            "hello||world|| this is&&a test||reactive-&&-graph|| kubectl".to_string(),
        ),
        VariableValue::List(vec![
            VariableValue::String("||".to_string()),
            VariableValue::Integer(123.into()),
        ]),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}

#[tokio::test]
async fn test_split_too_many_args() {
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello,world".to_string()),
        VariableValue::String(",".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_split_too_few_args() {
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("hello,world".to_string())];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_split_multiple_delimiter() {
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hello,wor!ld".to_string()),
        VariableValue::List(vec![
            VariableValue::String(",".to_string()),
            VariableValue::String("!".to_string()),
        ]),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(
        result.unwrap(),
        VariableValue::List(vec![
            VariableValue::String("hello".to_string()),
            VariableValue::String("wor".to_string()),
            VariableValue::String("ld".to_string())
        ])
    );

    //Delimiters with multiple characters
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let args = vec![
        VariableValue::String(
            "hello||world|| this is&&a test||reactive-&&-graph|| kubectl".to_string(),
        ),
        VariableValue::List(vec![
            VariableValue::String("||".to_string()),
            VariableValue::String("&&".to_string()),
        ]),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(
        result.unwrap(),
        VariableValue::List(vec![
            VariableValue::String("hello".to_string()),
            VariableValue::String("world".to_string()),
            VariableValue::String(" this is".to_string()),
            VariableValue::String("a test".to_string()),
            VariableValue::String("reactive-".to_string()),
            VariableValue::String("-graph".to_string()),
            VariableValue::String(" kubectl".to_string())
        ])
    );
}

#[tokio::test]
async fn test_split_multiple_delimiter_invalid_inputs() {
    let split = text::Split {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String(
            "hello||world|| this is&&a test||reactive-&&-graph|| kubectl".to_string(),
        ),
        VariableValue::List(vec![
            VariableValue::String("||".to_string()),
            VariableValue::Integer(123.into()),
        ]),
    ];
    let result = split.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}
