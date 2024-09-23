use std::sync::Arc;

use super::temporal_instant;
use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};
use chrono::NaiveTime;
use drasi_query_ast::ast;

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_truncate_hour() {
    let truncate_localtime = temporal_instant::Truncate {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::String("hour".to_string()),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 51, 12).unwrap()),
    ];
    let result = truncate_localtime
        .call(&context, &get_func_expr(), args.clone())
        .await;
    assert_eq!(
        result.unwrap(),
        VariableValue::LocalTime(NaiveTime::from_hms_opt(9, 0, 0).unwrap())
    );
}
