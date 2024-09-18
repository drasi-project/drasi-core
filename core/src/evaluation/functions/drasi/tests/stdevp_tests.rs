use std::sync::Arc;

use crate::evaluation::functions::drasi::DrasiStdevP;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::ExpressionEvaluationContext;
use crate::evaluation::{context::QueryVariables, InstantQueryClock};
use approx::assert_relative_eq;
use drasi_query_ast::ast;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[allow(clippy::approx_constant)]
#[tokio::test]
async fn test_drasi_stdevp() {
    let stdevp = DrasiStdevP {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Integer(1.into()),
        VariableValue::Float((3.1415).into()),
        VariableValue::Integer((-12).into()),
        VariableValue::Float((9.0).into()),
    ])];

    let result = stdevp
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_relative_eq!(
        result.as_f64().unwrap(),
        6.8645235493805,
        epsilon = 0.00000001
    );
}

#[allow(clippy::approx_constant)]
#[tokio::test]
async fn test_drasi_stdevp_with_null() {
    let stdevp = DrasiStdevP {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::List(vec![
        VariableValue::Integer(0.into()),
        VariableValue::Integer(1.into()),
        VariableValue::Float((3.1415).into()),
        VariableValue::Integer((-12).into()),
        VariableValue::Float((9.0).into()),
        VariableValue::Null,
    ])];

    let result = stdevp
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_relative_eq!(
        result.as_f64().unwrap(),
        6.8645235493805,
        epsilon = 0.00000001
    );

    let args = VariableValue::Null;
    let result = stdevp
        .call(&context, &get_func_expr(), vec![args])
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}
