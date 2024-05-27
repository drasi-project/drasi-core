use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::cypher_scalar::Head;
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
async fn test_head() {
    let head = Head {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::String("one".to_string()),
        VariableValue::String("two".to_string()),
        VariableValue::String("three".to_string()),
    ])];
    let result = head.call(&context, &get_func_expr(), args.clone()).await;
    assert_eq!(result.unwrap(), VariableValue::String("one".to_string()));
}

#[tokio::test]
async fn test_head_multiple_args() {
    let head = Head {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::List(vec![
            VariableValue::String("one".to_string()),
            VariableValue::String("two".to_string()),
            VariableValue::String("three".to_string()),
        ]),
        VariableValue::Integer(3.into()),
        VariableValue::List(vec![
            VariableValue::String("four".to_string()),
            VariableValue::String("five".to_string()),
            VariableValue::String("six".to_string()),
        ]),
    ];
    let result = head.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        EvaluationError::InvalidArgumentCount(_)
    ));
}
