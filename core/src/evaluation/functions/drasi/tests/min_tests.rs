use std::sync::Arc;

use crate::evaluation::functions::drasi::DrasiMin;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::{context::QueryVariables, InstantQueryClock};
use drasi_query_ast::ast;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_drasi_min() {
    let min = DrasiMin {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Integer(1.into()),
    ])];

    let result = min
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0);
}

#[tokio::test]
async fn test_drasi_min_multiple_types() {
    let min = DrasiMin {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Null,
        VariableValue::Integer(1.into()),
        VariableValue::Float((3.1415).into()),
        VariableValue::String("test".into()),
        VariableValue::Bool(true),
    ])];

    let result = min
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Bool(true));
}

#[tokio::test]
async fn test_drasi_min_list_of_strings() {
    let min = DrasiMin {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::String("apple".into()),
        VariableValue::String("banana".into()),
    ])];

    let result = min
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, "apple");
}
