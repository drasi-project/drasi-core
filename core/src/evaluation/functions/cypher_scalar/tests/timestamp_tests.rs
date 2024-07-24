use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::cypher_scalar::{timestamp};
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::{ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("function"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn test_timestamp() {
    let timestamp = timestamp::Timestamp {};

    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];
    let result = match timestamp.call(&context, &get_func_expr(), args.clone()).await {
        Ok(result) => result,
        Err(e) => panic!("Error: {:?}", e),
    };

    let time_since_epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let time_since_epoch = time_since_epoch.as_millis() as i64;

    //abs diff should be less than 500
    assert!((result.as_i64().unwrap() - time_since_epoch).abs() < 500);
}