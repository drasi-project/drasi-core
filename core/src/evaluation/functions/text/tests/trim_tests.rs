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
async fn test_trim() {
    let trim = text::Trim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("  hello  ".to_string())];
    let result = trim.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("hello".to_string()));

    let ltrim = text::LTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("  NULL ".to_string())];
    let result = ltrim.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("NULL ".to_string()));

    let rtrim = text::RTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::String("  hello  ".to_string())];
    let result = rtrim.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(
        result.unwrap(),
        VariableValue::String("  hello".to_string())
    );
}

#[tokio::test]
async fn test_trim_too_many_args() {
    let trim = text::Trim {};
    let ltrim = text::LTrim {};
    let rtrim = text::RTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("  hello  ".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = trim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));

    let args = vec![
        VariableValue::String("  hello  ".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = ltrim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));

    let args = vec![
        VariableValue::String("  hello  ".to_string()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = rtrim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_trim_invalid_inputs() {
    let trim = text::Trim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = trim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let ltrim = text::LTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = ltrim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let rtrim = text::RTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = rtrim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}

#[tokio::test]
async fn test_trim_too_few_args() {
    let trim = text::Trim {};
    let ltrim = text::LTrim {};
    let rtrim = text::RTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = trim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));

    let args = vec![];
    let result = ltrim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));

    let args = vec![];
    let result = rtrim.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_trim_null() {
    let trim = text::Trim {};
    let ltrim = text::LTrim {};
    let rtrim = text::RTrim {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];
    let result = trim.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![VariableValue::Null];
    let result = ltrim.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![VariableValue::Null];
    let result = rtrim.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}
