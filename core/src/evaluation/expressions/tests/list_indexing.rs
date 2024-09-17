use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::variable_value::integer::Integer;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{EvaluationError, InstantQueryClock};
use crate::evaluation::{ExpressionEvaluationContext, ExpressionEvaluator};
use crate::in_memory_index::in_memory_result_index::InMemoryResultIndex;
use std::sync::Arc;

#[tokio::test]
async fn test_list_indexing() {
    let expr = "[5,1,7][1]";

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
        VariableValue::Integer(Integer::from(1))
    );
}

#[tokio::test]
async fn test_list_indexing_negative_index() {
    let expr = "[5,1,7,12,-3423,79][-3]";

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
        VariableValue::Integer(Integer::from(12))
    );
}

#[tokio::test]
async fn test_list_indexing_expression_index() {
    let expr = "[5,1,7,12,-3423,79][1+2]";

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
        VariableValue::Integer(Integer::from(12))
    );
}

#[tokio::test]
async fn test_list_indexing_range_index() {
    let expr = "[5,1,7,12,-3423,79][2..5]";

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
            VariableValue::Integer(Integer::from(7)),
            VariableValue::Integer(Integer::from(12)),
            VariableValue::Integer(Integer::from(-3423))
        ])
    );
}

#[tokio::test]
async fn test_list_indexing_range_index_unbounded_end() {
    let expr = "[5,1,7,12,-3423,79][2..]";

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
            VariableValue::Integer(Integer::from(7)),
            VariableValue::Integer(Integer::from(12)),
            VariableValue::Integer(Integer::from(-3423)),
            VariableValue::Integer(Integer::from(79))
        ])
    );
}

#[tokio::test]
async fn test_list_indexing_range_index_unbounded_start() {
    let expr = "[5,1,7,12,-3423,79][..4]";

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
            VariableValue::Integer(Integer::from(5)),
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(7)),
            VariableValue::Integer(Integer::from(12)),
        ])
    );
}

#[tokio::test]
async fn test_list_indexing_range_index_out_of_bound_slicing() {
    let expr = "[5,1,7,12,-3423,79][1..10]";

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
            VariableValue::Integer(Integer::from(1)),
            VariableValue::Integer(Integer::from(7)),
            VariableValue::Integer(Integer::from(12)),
            VariableValue::Integer(Integer::from(-3423)),
            VariableValue::Integer(Integer::from(79)),
        ])
    );
}

#[tokio::test]
async fn test_list_indexing_out_of_bound_indexing() {
    let expr = "[5,1,7,12,-3423,79][32]";

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
async fn test_list_indexing_string_indexing() {
    let expr = "[5,1,7,12,-3423,79]['abc']";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert!(matches!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap_err(),
        EvaluationError::InvalidType { expected: _ },
    ));
}

#[tokio::test]
async fn test_list_indexing_null_indexing() {
    let expr = "[5,1,7,12,-3423,79][null]";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert!(matches!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap_err(),
        EvaluationError::InvalidType { expected: _ },
    ));
}

#[tokio::test]
async fn test_list_indexing_float_indexing() {
    let expr = "[5,1,7,12,-3423,79][-1.5]";

    let expr = drasi_query_cypher::parse_expression(expr).unwrap();

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let evaluator = ExpressionEvaluator::new(function_registry.clone(), ari.clone());

    let variables = QueryVariables::new();

    let context =
        ExpressionEvaluationContext::new(&variables, Arc::new(InstantQueryClock::new(0, 0)));
    assert!(matches!(
        evaluator
            .evaluate_expression(&context, &expr)
            .await
            .unwrap_err(),
        EvaluationError::InvalidType { expected: _ },
    ));
}

#[tokio::test]
async fn test_list_indexing_range_index_negative_start_bound() {
    let expr = "[0,1,2,3,4,5,6,7,8,9,10][-7..]";

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
            VariableValue::Integer(Integer::from(8)),
            VariableValue::Integer(Integer::from(9)),
            VariableValue::Integer(Integer::from(10)),
        ])
    );
}

#[tokio::test]
async fn test_list_indexing_range_index_negative_end_bound() {
    let expr = "[0,1,2,3,4,5,6,7,8,9,10][..-5]";

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
        ])
    );
}
