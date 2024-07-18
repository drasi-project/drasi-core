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
async fn test_substring() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(1.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::String("rasi".to_string()));

    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasiReactivegraph".to_string()),
        VariableValue::Integer(4.into()),
        VariableValue::Integer(9.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::String("iReactive".to_string())
    );
}

#[tokio::test]
async fn test_substring_zero_length() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(1.into()),
        VariableValue::Integer(0.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::String("".to_string()));

}

#[tokio::test]
async fn test_substring_too_many_args() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(1.into()),
        VariableValue::Integer(2.into()),
        VariableValue::String("WORLD".to_string()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_substring_too_few_args() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_substring_invalid_input_values() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Integer(10.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let args = vec![
        VariableValue::String("drasiReactivegraph".to_string()),
        VariableValue::Integer(4.into()),
        VariableValue::Integer(19.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let args = vec![
        VariableValue::String("drasiReactivegraph".to_string()),
        VariableValue::Integer((-1).into()),
        VariableValue::Integer(4.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));

    let args = vec![
        VariableValue::String("drasiReactivegraph".to_string()),
        VariableValue::Integer(4.into()),
        VariableValue::Integer((-1).into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}

#[tokio::test]
async fn test_substring_invalid_inputs() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::String("reactive".to_string()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}


#[tokio::test]
async fn test_substring_null() {
    let substring = text::Substring {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Null,
        VariableValue::Integer(1.into()),
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let args = vec![
        VariableValue::String("drasi".to_string()),
        VariableValue::Null,
    ];
    let result = substring
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(result.unwrap_err(), EvaluationError::InvalidType));
}