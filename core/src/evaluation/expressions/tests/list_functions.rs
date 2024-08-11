use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::variable_value::float::Float;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    FunctionError, EvaluationError, FunctionEvaluationError, ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock,
};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;

use std::sync::Arc;

#[tokio::test]
async fn test_list_tail() {
    let expr = "tail(['one','two','three'])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::String("two".to_string()),
            VariableValue::String("three".to_string())
        ])
    );
}

#[tokio::test]
async fn test_list_tail_multiple_arguments() {
    let expr = "tail(['one','two','three'], 3, ['four','five','six'])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();


    assert!(matches!(
        result,
        EvaluationError::FunctionError(
            FunctionError {
                function_name: _name,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        )
    ));
}

#[tokio::test]
async fn test_list_tail_null() {
    let expr = "tail(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::Null
    );
}

#[tokio::test]
async fn test_list_reverse() {
    let expr = "reverse(['one','two',null, 'null', 123, 4.23])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Float(Float::from(4.23)),
            VariableValue::Integer(Integer::from(123)),
            VariableValue::String("null".to_string()),
            VariableValue::Null,
            VariableValue::String("two".to_string()),
            VariableValue::String("one".to_string())
        ])
    );
}

#[tokio::test]
async fn test_list_reverse_null() {
    let expr = "reverse(null)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::Null
    );
}

#[tokio::test]
async fn test_list_reverse_multiple_arguments() {
    let expr = "reverse(['one','two','three'], 3, ['four','five','six'])";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        )
    ));
}

#[tokio::test]
async fn test_list_range() {
    let expr = "range(0,10)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(0)),
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(3)),
            VariableValue::Integer(Integer::from(4)),
            VariableValue::Integer(Integer::from(5)),
            VariableValue::Integer(Integer::from(6)),
            VariableValue::Integer(Integer::from(7)),
            VariableValue::Integer(Integer::from(8)),
            VariableValue::Integer(Integer::from(9)),
            VariableValue::Integer(Integer::from(10))
        ])
    );
}

#[tokio::test]
async fn test_list_range_step() {
    let expr = "range(2,18,3)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert_eq!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap(),
        VariableValue::List(vec![
            VariableValue::Integer(Integer::from(2)),
            VariableValue::Integer(Integer::from(5)),
            VariableValue::Integer(Integer::from(8)),
            VariableValue::Integer(Integer::from(11)),
            VariableValue::Integer(Integer::from(14)),
            VariableValue::Integer(Integer::from(17))
        ])
    );
}

#[tokio::test]
async fn test_list_range_invalid_arg_count() {
    let expr = "range(2,18,3,4)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();
    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();


    assert!(matches!(
        result,
        EvaluationError::FunctionError(
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        )
    ));

    let expr = "range(2)";
    let expr = drasi_query_cypher::parse_expression(expr).unwrap();
    let result = evaluator
        .evaluate_expression(&context, &expr)
        .await
        .unwrap_err();

    assert!(matches!(
        result,
        EvaluationError::FunctionError(
            FunctionError {
                function_name: _,
                error: FunctionEvaluationError::InvalidArgumentCount
            }
        )
    ));
}
