use std::sync::Arc;

use crate::evaluation::functions::drasi::DrasiMax;
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
async fn test_drasi_max() {
    let max = DrasiMax {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Float((9.0).into()),
    ])];

    let result = max
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 9.0);
}

#[allow(clippy::float_cmp)]
#[tokio::test]
async fn test_drasi_max_multiple_types() {
    let max = DrasiMax {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Null,
        VariableValue::Integer(1.into()),
        VariableValue::Float((std::f64::consts::PI).into()),
        VariableValue::String("test".into()),
        VariableValue::Bool(true),
    ])];

    let result = max
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, std::f64::consts::PI);
}

#[tokio::test]
async fn test_drasi_max_list_of_strings() {
    let max = DrasiMax {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::String("apple".into()),
        VariableValue::String("banana".into()),
    ])];

    let result = max
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, "banana");
}
