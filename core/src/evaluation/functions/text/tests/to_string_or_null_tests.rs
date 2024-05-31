use drasi_query_ast::ast;

use super::text;
use std::collections::BTreeMap;
use std::sync::Arc;

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
async fn test_to_string_null() {
    let to_string = text::ToStringOrNull {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Integer(123.into())];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::String("123".to_string()));

    let to_string = text::ToStringOrNull {};
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

    let to_string = text::ToStringOrNull {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let map: BTreeMap<String, VariableValue> = BTreeMap::from([
        (
            "key1".to_string(),
            VariableValue::String("value1".to_string()),
        ),
        (
            "foo2".to_string(),
            VariableValue::String("bar2".to_string()),
        ),
    ])
    .clone();
    let args = vec![VariableValue::Object(map)];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);

    let to_string = text::ToStringOrNull {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));
    let args = vec![VariableValue::Null];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(result.unwrap(), VariableValue::Null);
}

#[tokio::test]
async fn test_to_string_null_too_many_args() {
    let to_string = text::ToStringOrNull {};
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
        EvaluationError::InvalidArgumentCount(_)
    ));
}

#[tokio::test]
async fn test_to_string_null_too_few_args() {
    let to_string = text::ToStringOrNull {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = to_string
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}
