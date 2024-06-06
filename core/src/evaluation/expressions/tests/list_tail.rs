use crate::evaluation::context::QueryVariables;
use crate::evaluation::functions::FunctionRegistry;
use crate::evaluation::variable_value::VariableValue;
use crate::evaluation::{
    EvaluationError, ExpressionEvaluationContext, ExpressionEvaluator, InstantQueryClock,
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

    let err = Box::new(EvaluationError::InvalidArgumentCount("tail".to_string()));

    assert!(matches!(
        result,
        EvaluationError::FunctionError {
            function_name: name,
            error: err
        }
    ));
}
