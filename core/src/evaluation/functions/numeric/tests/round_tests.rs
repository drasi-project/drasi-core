use std::sync::Arc;

use drasi_query_ast::ast;

use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::numeric::Round;
use crate::evaluation::functions::ScalarFunction;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{FunctionError, FunctionEvaluationError, ExpressionEvaluationContext, InstantQueryClock};

fn get_func_expr() -> ast::FunctionExpression {
    ast::FunctionExpression {
        name: Arc::from("round"),
        args: vec![],
        position_in_query: 10,
    }
}

#[tokio::test]
async fn round_too_few_args() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![];

    let result = round.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount,
        } 
    ));
}

#[tokio::test]
async fn round_too_many_args() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Integer(Integer::from(0)),
        VariableValue::Integer(Integer::from(0)),
        VariableValue::Integer(Integer::from(0)),
        VariableValue::Integer(Integer::from(0)),
    ];

    let result = round.call(&context, &get_func_expr(), args.clone()).await;
    assert!(matches!(
        result.unwrap_err(),
        FunctionError {
            function_name: _,
            error: FunctionEvaluationError::InvalidArgumentCount,
        }
    ));
}

#[tokio::test]
async fn round_null() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Null];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, VariableValue::Null);
}

#[tokio::test]
async fn round_zero_float() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(0_f64).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn round_negative_int() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-1_f64).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1_f64);
}

#[tokio::test]
async fn round_negative_float_up() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-0.001).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 0_f64);
}

#[tokio::test]
async fn round_negative_float_down() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(-0.7).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1_f64);
}

#[tokio::test]
async fn round_positive_int() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1_f64).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1_f64);
}

#[tokio::test]
async fn round_positive_float_up() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1.6).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 2_f64);
}

#[tokio::test]
async fn round_positive_float_down() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![VariableValue::Float(Float::from_f64(1.001).unwrap())];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1_f64);
}

#[tokio::test]
async fn round_positive_float_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(3.1415926).unwrap()),
        VariableValue::Integer(Integer::from(3)),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 3.142_f64);
}

#[tokio::test]
async fn round_negative_float_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.5).unwrap()),
        VariableValue::Integer(Integer::from(0)),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.0_f64);
}

#[tokio::test]
async fn round_negative_float_ties_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.5555).unwrap()),
        VariableValue::Integer(Integer::from(3)),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.555_f64);
}

#[tokio::test]
async fn round_positive_float_up_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.3_f64);
}

#[tokio::test]
async fn round_negative_float_up_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.3_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.35).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.4_f64);
}

#[tokio::test]
async fn round_positive_float_down_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("DOWN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);
}

#[tokio::test]
async fn round_negative_float_down_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("DOWN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.2_f64);
}

#[tokio::test]
async fn round_positive_float_ceiling_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("CEILING".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.3_f64);
}

#[tokio::test]
async fn round_negative_float_ceiling_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("CEILING".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.2_f64);
}

#[tokio::test]
async fn round_positive_float_floor_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("FLOOR".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);
}

#[tokio::test]
async fn round_negative_float_floor_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("FLOOR".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.3_f64);
}

#[tokio::test]
async fn round_positive_float_half_up_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(1.25).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.3_f64);
}

#[tokio::test]
async fn round_negative_float_half_up_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.3_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.35).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_UP".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.4_f64);
}

#[tokio::test]
async fn round_positive_float_half_down_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_DOWN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(1.25).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_DOWN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);
}

#[tokio::test]
async fn round_negative_float_half_down_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_DOWN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.3_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.35).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_DOWN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.3_f64);
}

#[tokio::test]
async fn round_positive_float_half_even_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(1.249).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_EVEN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(1.25).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_EVEN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.2_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(1.222335).unwrap()),
        VariableValue::Integer(Integer::from(5)),
        VariableValue::String("HALF_EVEN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, 1.22234_f64);
}

#[tokio::test]
async fn round_negative_float_half_even_mode_with_precision() {
    let round = Round {};
    let binding = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&binding, Arc::new(InstantQueryClock::new(0, 0)));

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.251).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_EVEN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.3_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.35).unwrap()),
        VariableValue::Integer(Integer::from(1)),
        VariableValue::String("HALF_EVEN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.4_f64);

    let args = vec![
        VariableValue::Float(Float::from_f64(-1.23455).unwrap()),
        VariableValue::Integer(Integer::from(4)),
        VariableValue::String("HALF_EVEN".to_string()),
    ];

    let result = round
        .call(&context, &get_func_expr(), args.clone())
        .await
        .unwrap();
    assert_eq!(result, -1.2346_f64);
}
