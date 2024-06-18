use std::sync::Arc;

use super::process_solution;

use crate::{
    evaluation::{
        context::PhaseEvaluationContext, functions::FunctionRegistry, phases::tests::build_query, variable_value::VariableValue, ExpressionEvaluator, QueryPhaseEvaluator
    },
    in_memory_index::in_memory_result_index::InMemoryResultIndex,
};

use serde_json::json;

#[tokio::test]
async fn add_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 = 1 RETURN a");

    let node1 = VariableValue::from(json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    }));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()]
        }]
    );
}

#[tokio::test]
async fn update_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 < 10 RETURN a");

    let node1 = VariableValue::from(json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    }));

    let node2 = VariableValue::from(json!({
      "id": 1,
      "Value1": 2,
      "Name": "bar"
    }));

    let node3 = VariableValue::from(json!({
      "id": 1,
      "Value1": 10,
      "Name": "bar"
    }));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node2.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node1.clone()],
            after: variablemap!["a" => node2.clone()]
        }]
    );

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Updating {
            before: variablemap!["a" => node2.clone()],
            after: variablemap!["a" => node3.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node2.clone()],
        }]
    );
}

#[tokio::test]
async fn remove_solution() {
    let query = build_query("MATCH (a) WHERE a.Value1 = 1 RETURN a");

    let node1 = VariableValue::from(json!({
      "id": 1,
      "Value1": 1,
      "Name": "foo"
    }));

    let function_registry = Arc::new(FunctionRegistry::new());
    let ari = Arc::new(InMemoryResultIndex::new());
    let expr_evaluator = Arc::new(ExpressionEvaluator::new(
        function_registry.clone(),
        ari.clone(),
    ));
    let evaluator = Arc::new(QueryPhaseEvaluator::new(
        expr_evaluator.clone(),
        ari.clone(),
    ));

    process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Adding {
            after: variablemap!["a" => node1.clone()],
        },
    )
    .await;

    let result = process_solution(
        &query,
        &evaluator,
        PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()],
        },
    );

    assert_eq!(
        result.await,
        vec![PhaseEvaluationContext::Removing {
            before: variablemap!["a" => node1.clone()]
        }]
    );
}
